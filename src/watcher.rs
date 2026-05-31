use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::oauth::{OAuthAdapter, OAuthAdapterConfig};
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::{FailedAdapter, McpAdapter, StartingAdapter};
use crate::config::{self, Config, ConfigDiff, EndpointConfig, Transport};
use crate::events::ToolCallEventBus;
use crate::oauth::OAuthFlowManager;
use crate::profile_registry::ProfileRegistry;
use crate::registry::AdapterRegistry;
use crate::token_manager::TokenManager;
use crate::OAuthAdapterInners;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tracing::{error, info, warn, Instrument};

/// Watches a config file for changes and applies diffs to the adapter registry.
pub struct ConfigWatcher;

impl ConfigWatcher {
    /// Start watching `config_path` for modifications.
    ///
    /// On each detected change (debounced by 500ms), the config is reloaded,
    /// diffed against the previous version, and the diff is applied to the
    /// registry (adding/removing/restarting adapters as needed).
    ///
    /// Returns a `JoinHandle` for the background task.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        config_path: PathBuf,
        registry: Arc<AdapterRegistry>,
        machine_name: String,
        js_execution_mode: Arc<AtomicBool>,
        profile_registry: Arc<ProfileRegistry>,
        token_manager: Arc<TokenManager>,
        _oauth_flow_manager: Arc<OAuthFlowManager>,
        oauth_adapter_inners: OAuthAdapterInners,
        shared_config: Arc<RwLock<Config>>,
        event_bus: Option<ToolCallEventBus>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = watch_loop(
                config_path,
                registry,
                machine_name,
                js_execution_mode,
                profile_registry,
                token_manager,
                oauth_adapter_inners,
                shared_config,
                event_bus,
            )
            .await
            {
                error!(error = %e, "Config watcher terminated with error");
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn watch_loop(
    config_path: PathBuf,
    registry: Arc<AdapterRegistry>,
    _machine_name: String,
    js_execution_mode: Arc<AtomicBool>,
    profile_registry: Arc<ProfileRegistry>,
    token_manager: Arc<TokenManager>,
    oauth_adapter_inners: OAuthAdapterInners,
    shared_config: Arc<RwLock<Config>>,
    event_bus: Option<ToolCallEventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    let _ = tx.blocking_send(());
                }
            }
        },
        notify::Config::default(),
    )?;

    // Watch the parent directory so renames/atomic writes are caught
    let watch_path = config_path.parent().unwrap_or(&config_path).to_path_buf();
    watcher.watch(&watch_path, RecursiveMode::NonRecursive)?;

    info!(path = %config_path.display(), "Config watcher started");

    // Baseline lives in `shared_config`, populated at startup by `main.rs`
    // (and re-populated by every successful `reload_and_apply` below). No
    // separate initial parse here — that would race with main.rs's load and
    // produce two distinct in-memory copies of the same TOML.

    loop {
        // Wait for a filesystem event
        if rx.recv().await.is_none() {
            break; // channel closed
        }

        // Debounce: drain further events for 500ms
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(())) => continue,  // more events, keep draining
                Ok(None) => return Ok(()), // channel closed
                Err(_) => break,           // timeout expired, proceed
            }
        }

        info!(path = %config_path.display(), "Config file change detected, reloading");

        let _ = reload_and_apply(
            &config_path,
            &shared_config,
            &registry,
            &js_execution_mode,
            &profile_registry,
            &token_manager,
            &oauth_adapter_inners,
            event_bus.as_ref(),
        )
        .await;
    }

    Ok(())
}

/// Reload `config_path` from disk and reconcile it into the running relay.
///
/// Performs **keep-last-good** semantics: if the file fails to parse OR fails
/// fail-fast profile validation (`[[profiles]]` block — see
/// [`config::validate_profiles`]), the previous registry state is preserved
/// untouched and an `Err` is returned with the parse/validation error logged.
/// Per-endpoint validation failures are surfaced as warnings (FailedAdapter
/// registrations) rather than aborting the reload, matching the pre-existing
/// hot-reload behaviour.
///
/// Returns `Ok(())` when the reload was applied (including warning-only
/// reloads); returns `Err(ConfigError)` when nothing was applied because the
/// new config could not be parsed or its profile block was invalid.
#[allow(clippy::too_many_arguments)]
async fn reload_and_apply(
    config_path: &Path,
    shared_config: &Arc<RwLock<Config>>,
    registry: &Arc<AdapterRegistry>,
    js_execution_mode: &Arc<AtomicBool>,
    profile_registry: &Arc<ProfileRegistry>,
    token_manager: &Arc<TokenManager>,
    oauth_adapter_inners: &OAuthAdapterInners,
    event_bus: Option<&ToolCallEventBus>,
) -> Result<(), config::ConfigError> {
    // Parse new config gracefully. Fatal errors (parse failure or
    // fail-fast profile-validation failure) bail out without touching the
    // running registry — that is the keep-last-good guarantee. `shared_config`
    // (also read by `ManagementState`) is left untouched on this path.
    let (new_config, warnings) = match config::load_config_graceful(config_path) {
        Ok(result) => result,
        Err(e) => {
            warn!(error = %e, "Failed to parse updated config, keeping current config");
            return Err(e);
        }
    };

    for w in &warnings {
        warn!("{}", w);
    }

    let warned_names = config::warned_endpoint_names(&warnings);

    // Snapshot the previous config for diffing. Cloning lets us drop the
    // read lock before the (potentially slow) `apply_diff_graceful` spawns
    // run, so management API readers are never blocked on adapter init.
    let old_config = shared_config.read().await.clone();
    let diff = config::diff_configs(&old_config, &new_config);
    drop(old_config);

    apply_diff_graceful(
        &diff,
        registry,
        &warnings,
        &warned_names,
        token_manager,
        oauth_adapter_inners,
        new_config.relay.allow_insecure_oauth.unwrap_or(false),
        event_bus,
    )
    .await;

    // Update JS execution mode flag if it changed.
    let new_js_mode = new_config.relay.local_js_execution.unwrap_or(false);
    let old_js_mode = js_execution_mode.load(Ordering::Relaxed);
    if new_js_mode != old_js_mode {
        js_execution_mode.store(new_js_mode, Ordering::Relaxed);
        info!(js_execution_mode = new_js_mode, "JS execution mode updated");
    }

    // Rebuild the profile registry from the reloaded config. Per recon §D4
    // the watcher is the source of truth for profile state, mirroring how
    // `js_execution_mode` is propagated above. `rebuild` performs a single
    // write-lock swap so requests in flight see either the old or the new
    // map atomically.
    profile_registry
        .rebuild(new_config.profiles.as_deref().unwrap_or(&[]))
        .await;

    // Publish the new baseline to the shared handle so the next
    // management-API read sees it (and the next diff is taken against it).
    *shared_config.write().await = new_config;
    Ok(())
}

/// Apply a config diff to the adapter registry.
///
/// This is public so it can also be called from a manual reload endpoint.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub async fn apply_diff(
    diff: &ConfigDiff,
    registry: &AdapterRegistry,
    token_manager: &Arc<TokenManager>,
    oauth_adapter_inners: &OAuthAdapterInners,
    allow_insecure_oauth: bool,
    event_bus: Option<&ToolCallEventBus>,
) {
    // Remove endpoints
    for name in &diff.removed {
        let span = tracing::info_span!("endpoint", endpoint = %name);
        let _g = span.enter();
        info!("Removing endpoint");
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(error = %e, "Error shutting down removed adapter");
            }
        }
    }

    // Changed endpoints: shutdown old, register as Starting, init in background
    for (name, new_ep) in &diff.changed {
        let span = tracing::info_span!("endpoint", endpoint = %name);
        let _g = span.enter();
        info!("Restarting changed endpoint");
        let (was_disabled, old_disabled_tools) = {
            let entries = registry.entries().read().await;
            if let Some(entry) = entries.get(name.as_str()) {
                (entry.disabled, entry.disabled_tools.clone())
            } else {
                (
                    new_ep.disabled,
                    new_ep.disabled_tools.iter().cloned().collect(),
                )
            }
        };
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(error = %e, "Error shutting down old adapter");
            }
        }

        // Register immediately with Starting status
        registry
            .register(
                name.clone(),
                Box::new(StartingAdapter),
                new_ep.transport.to_string(),
                new_ep.description.clone(),
                new_ep.resolved_tool_prefix(),
            )
            .await;
        {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(name.as_str()) {
                entry.disabled = was_disabled;
                entry.disabled_tools = old_disabled_tools.clone();
            }
        }

        // Spawn background initialization
        let reg = registry.clone();
        let ep_clone = new_ep.clone();
        let name_clone = name.clone();
        let tm = token_manager.clone();
        let oai = oauth_adapter_inners.clone();
        let bus = event_bus.cloned();
        let init_span = tracing::info_span!("endpoint", endpoint = %name_clone);
        tokio::spawn(
            async move {
                let adapter =
                    create_adapter(&ep_clone, &tm, &oai, allow_insecure_oauth, bus.as_ref()).await;
                let mut entries = reg.entries().write().await;
                if let Some(entry) = entries.get_mut(name_clone.as_str()) {
                    entry.adapter = adapter;
                    if was_disabled {
                        let _ = entry.adapter.shutdown().await;
                    }
                }
                drop(entries);
                reg.rewire_tools_changed_listener(&name_clone).await;
                reg.invalidate_endpoint_tool_cache(&name_clone).await;
                info!("Changed endpoint initialized");
            }
            .instrument(init_span),
        );
    }

    // Added endpoints
    for ep in &diff.added {
        let span = tracing::info_span!("endpoint", endpoint = %ep.name);
        let _g = span.enter();
        info!(transport = %ep.transport, "Adding new endpoint");

        // Register immediately with Starting status
        registry
            .register(
                ep.name.clone(),
                Box::new(StartingAdapter),
                ep.transport.to_string(),
                ep.description.clone(),
                ep.resolved_tool_prefix(),
            )
            .await;
        if ep.disabled || !ep.disabled_tools.is_empty() {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                entry.disabled = ep.disabled;
                entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
            }
        }

        // Spawn background initialization
        let reg = registry.clone();
        let ep_clone = ep.clone();
        let tm = token_manager.clone();
        let oai = oauth_adapter_inners.clone();
        let bus = event_bus.cloned();
        let init_span = tracing::info_span!("endpoint", endpoint = %ep_clone.name);
        tokio::spawn(
            async move {
                let adapter =
                    create_adapter(&ep_clone, &tm, &oai, allow_insecure_oauth, bus.as_ref()).await;
                let mut entries = reg.entries().write().await;
                if let Some(entry) = entries.get_mut(ep_clone.name.as_str()) {
                    entry.adapter = adapter;
                    if ep_clone.disabled {
                        let _ = entry.adapter.shutdown().await;
                    }
                }
                drop(entries);
                reg.rewire_tools_changed_listener(&ep_clone.name).await;
                reg.invalidate_endpoint_tool_cache(&ep_clone.name).await;
                info!("New endpoint initialized");
            }
            .instrument(init_span),
        );
    }

    // Log unchanged
    for name in &diff.unchanged {
        let span = tracing::info_span!("endpoint", endpoint = %name);
        let _g = span.enter();
        info!("Endpoint unchanged, keeping running");
    }
}

/// Like [`apply_diff`] but also handles per-endpoint validation warnings.
///
/// Endpoints whose names appear in `warned_names` are registered as `FailedAdapter`
/// with the warning message instead of attempting initialization.
#[allow(clippy::too_many_arguments)]
pub async fn apply_diff_graceful(
    diff: &ConfigDiff,
    registry: &AdapterRegistry,
    warnings: &[config::EndpointValidationWarning],
    warned_names: &std::collections::HashSet<String>,
    token_manager: &Arc<TokenManager>,
    oauth_adapter_inners: &OAuthAdapterInners,
    allow_insecure_oauth: bool,
    event_bus: Option<&ToolCallEventBus>,
) {
    // Build warning message map
    let warning_messages: std::collections::HashMap<String, String> = {
        let mut map = std::collections::HashMap::new();
        for w in warnings {
            map.entry(w.endpoint_name.clone())
                .and_modify(|msg: &mut String| {
                    msg.push_str("; ");
                    msg.push_str(&w.message);
                })
                .or_insert_with(|| w.message.clone());
        }
        map
    };

    // Remove endpoints
    for name in &diff.removed {
        let span = tracing::info_span!("endpoint", endpoint = %name);
        let _g = span.enter();
        info!("Removing endpoint");
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(error = %e, "Error shutting down removed adapter");
            }
        }
    }

    // Changed endpoints: shutdown old, register as Starting, init in background
    for (name, new_ep) in &diff.changed {
        let span = tracing::info_span!("endpoint", endpoint = %name);
        let _g = span.enter();
        info!("Restarting changed endpoint");
        let (was_disabled, old_disabled_tools) = {
            let entries = registry.entries().read().await;
            if let Some(entry) = entries.get(name.as_str()) {
                (entry.disabled, entry.disabled_tools.clone())
            } else {
                (
                    new_ep.disabled,
                    new_ep.disabled_tools.iter().cloned().collect(),
                )
            }
        };
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(error = %e, "Error shutting down old adapter");
            }
        }

        // Warned endpoints get FailedAdapter immediately (no background init)
        if warned_names.contains(name) {
            let msg = warning_messages.get(name).cloned().unwrap_or_default();
            warn!("Registering as failed due to validation error: {}", msg);
            registry
                .register(
                    name.clone(),
                    Box::new(
                        FailedAdapter::new(msg)
                            .with_server_type_override(new_ep.server_type_override.clone()),
                    ),
                    new_ep.transport.to_string(),
                    new_ep.description.clone(),
                    new_ep.resolved_tool_prefix(),
                )
                .await;
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(name.as_str()) {
                entry.disabled = was_disabled;
                entry.disabled_tools = old_disabled_tools;
            }
            info!("Changed endpoint re-registered (failed)");
            continue;
        }

        // Register immediately with Starting status
        registry
            .register(
                name.clone(),
                Box::new(StartingAdapter),
                new_ep.transport.to_string(),
                new_ep.description.clone(),
                new_ep.resolved_tool_prefix(),
            )
            .await;
        {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(name.as_str()) {
                entry.disabled = was_disabled;
                entry.disabled_tools = old_disabled_tools.clone();
            }
        }

        // Spawn background initialization
        let reg = registry.clone();
        let ep_clone = new_ep.clone();
        let name_clone = name.clone();
        let tm = token_manager.clone();
        let oai = oauth_adapter_inners.clone();
        let bus = event_bus.cloned();
        let init_span = tracing::info_span!("endpoint", endpoint = %name_clone);
        tokio::spawn(
            async move {
                let adapter =
                    create_adapter(&ep_clone, &tm, &oai, allow_insecure_oauth, bus.as_ref()).await;
                let mut entries = reg.entries().write().await;
                if let Some(entry) = entries.get_mut(name_clone.as_str()) {
                    entry.adapter = adapter;
                    if was_disabled {
                        let _ = entry.adapter.shutdown().await;
                    }
                }
                drop(entries);
                reg.rewire_tools_changed_listener(&name_clone).await;
                reg.invalidate_endpoint_tool_cache(&name_clone).await;
                info!("Changed endpoint initialized");
            }
            .instrument(init_span),
        );
    }

    // Added endpoints
    for ep in &diff.added {
        let span = tracing::info_span!("endpoint", endpoint = %ep.name);
        let _g = span.enter();
        info!(transport = %ep.transport, "Adding new endpoint");

        // Warned endpoints get FailedAdapter immediately
        if warned_names.contains(&ep.name) {
            let msg = warning_messages.get(&ep.name).cloned().unwrap_or_default();
            warn!("Registering as failed due to validation error: {}", msg);
            registry
                .register(
                    ep.name.clone(),
                    Box::new(
                        FailedAdapter::new(msg)
                            .with_server_type_override(ep.server_type_override.clone()),
                    ),
                    ep.transport.to_string(),
                    ep.description.clone(),
                    ep.resolved_tool_prefix(),
                )
                .await;
            if ep.disabled || !ep.disabled_tools.is_empty() {
                let mut entries = registry.entries().write().await;
                if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                    entry.disabled = ep.disabled;
                    entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
                }
            }
            info!("New endpoint registered (failed)");
            continue;
        }

        // Register immediately with Starting status
        registry
            .register(
                ep.name.clone(),
                Box::new(StartingAdapter),
                ep.transport.to_string(),
                ep.description.clone(),
                ep.resolved_tool_prefix(),
            )
            .await;
        if ep.disabled || !ep.disabled_tools.is_empty() {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                entry.disabled = ep.disabled;
                entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
            }
        }

        // Spawn background initialization
        let reg = registry.clone();
        let ep_clone = ep.clone();
        let tm = token_manager.clone();
        let oai = oauth_adapter_inners.clone();
        let bus = event_bus.cloned();
        let init_span = tracing::info_span!("endpoint", endpoint = %ep_clone.name);
        tokio::spawn(
            async move {
                let adapter =
                    create_adapter(&ep_clone, &tm, &oai, allow_insecure_oauth, bus.as_ref()).await;
                let mut entries = reg.entries().write().await;
                if let Some(entry) = entries.get_mut(ep_clone.name.as_str()) {
                    entry.adapter = adapter;
                    if ep_clone.disabled {
                        let _ = entry.adapter.shutdown().await;
                    }
                }
                drop(entries);
                reg.rewire_tools_changed_listener(&ep_clone.name).await;
                reg.invalidate_endpoint_tool_cache(&ep_clone.name).await;
                info!("New endpoint initialized");
            }
            .instrument(init_span),
        );
    }

    // Log unchanged
    for name in &diff.unchanged {
        let span = tracing::info_span!("endpoint", endpoint = %name);
        let _g = span.enter();
        info!("Endpoint unchanged, keeping running");
    }
}

/// Resolve the OAuth client credentials for `ep`, preferring the DCR file
/// (managed by `TokenManager`) over the legacy `EndpointConfig` fields.
///
/// Wave 3a routes new credentials through `TokenManager::save_dcr`; the legacy
/// `client_id`/`client_secret` TOML fields remain readable for backwards
/// compatibility. When a TOML `client_secret` is the only source we emit a
/// one-time WARN so operators notice they should re-provision via the new
/// `POST /api/endpoints/{name}/credentials` route.
/// Build the conventional `{oauth_server_url}/token` endpoint, trimming a
/// single trailing slash on the base so that `https://accounts.google.com/`
/// produces `https://accounts.google.com/token` (not `…//token`). Used as
/// the defense-in-depth fallback when no explicit `token_endpoint` is
/// configured AND RFC 8414 discovery has not produced a URL.
pub(crate) fn conventional_token_endpoint(oauth_server_url: &str) -> String {
    format!("{}/token", oauth_server_url.trim_end_matches('/'))
}

/// Resolve the URL that an OAuth adapter should target for refresh-token
/// POSTs at construction time.
///
/// Priority:
/// 1. Explicit `ep.token_endpoint` from config (always wins).
/// 2. RFC 8414 discovery against `ep.oauth_server_url` — same code path the
///    management auth flow uses (`management.rs`).
/// 3. Slash-safe `{oauth_server_url}/token` conventional fallback.
///
/// Called from `main.rs` (initial endpoint registration) and from
/// `create_adapter` (config-watcher reconciliation). On discovery failure
/// emits a WARN matching the management flow style and continues with the
/// conventional URL so adapter construction never blocks indefinitely.
pub(crate) async fn resolve_oauth_token_endpoint(
    ep: &EndpointConfig,
    allow_insecure_oauth: bool,
) -> String {
    if let Some(explicit) = ep
        .token_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return explicit.to_string();
    }

    let Some(base) = ep
        .oauth_server_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return conventional_token_endpoint("");
    };

    match crate::oauth::discovery::discover_authorization_server(base, allow_insecure_oauth).await {
        Ok(disc) => {
            info!(
                endpoint = %ep.name,
                token_endpoint = %disc.token_endpoint,
                "RFC 8414 discovery resolved token endpoint at adapter init"
            );
            disc.token_endpoint
        }
        Err(e) => {
            let fallback = conventional_token_endpoint(base);
            warn!(
                endpoint = %ep.name,
                error = %e,
                fallback = %fallback,
                "RFC 8414 discovery against oauth_server_url failed at adapter init; falling back to convention-based token endpoint"
            );
            fallback
        }
    }
}

pub(crate) async fn resolve_oauth_client_creds(
    ep: &EndpointConfig,
    token_manager: &TokenManager,
) -> (String, Option<String>) {
    match token_manager.load_dcr(&ep.name).await {
        Ok(Some(creds)) => (creds.client_id, creds.client_secret),
        Ok(None) => {
            if ep.client_secret.is_some() {
                warn!(
                    endpoint = %ep.name,
                    "Using legacy `client_secret` from config.toml; \
                     re-provision via POST /api/endpoints/{{name}}/credentials \
                     to remove the secret from TOML"
                );
            }
            (
                ep.client_id.clone().unwrap_or_default(),
                ep.client_secret.clone(),
            )
        }
        Err(e) => {
            warn!(
                endpoint = %ep.name,
                error = %e,
                "Failed to read DCR credentials; falling back to config.toml"
            );
            (
                ep.client_id.clone().unwrap_or_default(),
                ep.client_secret.clone(),
            )
        }
    }
}

/// Create an adapter from an endpoint configuration.
///
/// Always returns an adapter. If initialization fails, returns a [`FailedAdapter`]
/// so the endpoint still appears in the registry with an unhealthy status.
pub(crate) async fn create_adapter(
    ep: &EndpointConfig,
    token_manager: &Arc<TokenManager>,
    oauth_adapter_inners: &OAuthAdapterInners,
    allow_insecure_oauth: bool,
    event_bus: Option<&ToolCallEventBus>,
) -> Box<dyn McpAdapter> {
    match ep.transport {
        Transport::Stdio => {
            let stdio_config = StdioConfig {
                command: ep.command.clone().unwrap_or_default(),
                args: ep.args.clone().unwrap_or_default(),
                env: ep.env.clone().unwrap_or_default(),
                server_type_override: ep.server_type_override.clone(),
                endpoint_name: ep.name.clone(),
            };
            let mut adapter = StdioAdapter::new(stdio_config);
            if let Some(bus) = event_bus {
                adapter.set_event_bus(bus.clone());
            }
            match adapter.initialize().await {
                Ok(()) => Box::new(adapter),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize stdio adapter, registering as failed");
                    Box::new(
                        FailedAdapter::new(e.to_string())
                            .with_server_type_override(ep.server_type_override.clone()),
                    )
                }
            }
        }
        Transport::Sse => {
            let url = ep.url.clone().unwrap_or_default();
            let mut sse_config = SseConfig::new(url);
            sse_config.headers = ep.headers.clone().unwrap_or_default();
            sse_config.server_type_override = ep.server_type_override.clone();
            sse_config.endpoint_name = ep.name.clone();
            let mut adapter = SseAdapter::new(sse_config);
            if let Some(bus) = event_bus {
                adapter.set_event_bus(bus.clone());
            }
            match adapter.initialize().await {
                Ok(()) => Box::new(adapter),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize SSE adapter, registering as failed");
                    Box::new(
                        FailedAdapter::new(e.to_string())
                            .with_server_type_override(ep.server_type_override.clone()),
                    )
                }
            }
        }
        Transport::Http => {
            let url = ep.url.clone().unwrap_or_default();
            let mut http_config = HttpConfig::new(url);
            http_config.headers = ep.headers.clone().unwrap_or_default();
            http_config.server_type_override = ep.server_type_override.clone();
            http_config.endpoint_name = ep.name.clone();
            let mut adapter = HttpAdapter::new(http_config);
            if let Some(bus) = event_bus {
                adapter.set_event_bus(bus.clone());
            }
            match adapter.initialize().await {
                Ok(()) => Box::new(adapter),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize HTTP adapter, registering as failed");
                    Box::new(
                        FailedAdapter::new(e.to_string())
                            .with_server_type_override(ep.server_type_override.clone()),
                    )
                }
            }
        }
        Transport::Oauth => {
            let (client_id, client_secret) =
                resolve_oauth_client_creds(ep, token_manager.as_ref()).await;
            let token_endpoint_url = resolve_oauth_token_endpoint(ep, allow_insecure_oauth).await;
            let oauth_config = OAuthAdapterConfig {
                endpoint_name: ep.name.clone(),
                url: ep.url.clone().unwrap_or_default(),
                token_endpoint_url,
                client_id,
                client_secret,
                heartbeat_interval_secs: 30,
                probe_timeout_secs: 10,
                probe_failure_threshold: 3,
                server_type_override: ep.server_type_override.clone(),
                allow_insecure_oauth,
            };

            let mut adapter = OAuthAdapter::new(oauth_config, token_manager.clone());
            if let Some(bus) = event_bus {
                adapter.set_event_bus(bus.clone());
            }
            let shared_inner = adapter.shared_inner();
            oauth_adapter_inners
                .write()
                .await
                .insert(ep.name.clone(), shared_inner);

            adapter.initialize().await.ok();
            info!(endpoint = %ep.name, "OAuth adapter initialized");
            Box::new(adapter)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::{broadcast, RwLock};

    /// A mock adapter that tracks whether shutdown was called.
    struct MockAdapter {
        health: HealthStatus,
        tools: Vec<ToolInfo>,
        shutdown_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl MockAdapter {
        fn healthy(
            tools: Vec<ToolInfo>,
            shutdown_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> Self {
            Self {
                health: HealthStatus::Healthy,
                tools,
                shutdown_called,
            }
        }
    }

    #[async_trait]
    impl McpAdapter for MockAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            name: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        fn health(&self) -> HealthStatus {
            self.health.clone()
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            self.shutdown_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// Mock adapter that exposes a tools-changed broadcast receiver. Used to
    /// verify that registry rewires its listener against the *new* adapter
    /// after an in-place swap.
    struct NotifyingMockAdapter {
        tools: Vec<ToolInfo>,
        tx: broadcast::Sender<()>,
    }

    impl NotifyingMockAdapter {
        fn new(tools: Vec<ToolInfo>, tx: broadcast::Sender<()>) -> Self {
            Self { tools, tx }
        }
    }

    #[async_trait]
    impl McpAdapter for NotifyingMockAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({}))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        fn subscribe_tools_changed(&self) -> Option<broadcast::Receiver<()>> {
            Some(self.tx.subscribe())
        }
    }

    fn make_tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{} tool", name)),
            input_schema: json!({"type": "object"}),
            annotations: None,
        }
    }

    fn endpoint_with_bad_command(name: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Stdio,
            command: Some("/nonexistent/binary/that/wont/start".to_string()),
            args: None,
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        }
    }

    fn empty_diff() -> ConfigDiff {
        ConfigDiff {
            added: vec![],
            removed: vec![],
            changed: vec![],
            unchanged: vec![],
        }
    }

    fn test_oauth_infra() -> (Arc<TokenManager>, OAuthAdapterInners) {
        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let inners = Arc::new(RwLock::new(HashMap::new()));
        // Leak the tempdir so it lives for the duration of the test
        std::mem::forget(tmp);
        (token_manager, inners)
    }

    #[tokio::test]
    async fn apply_diff_empty_is_noop() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "existing".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
                None,
                Some("existing".into()),
            )
            .await;

        apply_diff(&empty_diff(), &registry, &tm, &inners, false, None).await;

        // Existing adapter should still be there, not shut down
        assert!(!shutdown.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(registry.merged_catalog().await.len(), 1);
    }

    #[tokio::test]
    async fn apply_diff_removes_endpoint() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "to_remove".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
                None,
                Some("to_remove".into()),
            )
            .await;

        let diff = ConfigDiff {
            removed: vec!["to_remove".to_string()],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners, false, None).await;

        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
        assert!(registry.merged_catalog().await.is_empty());
    }

    #[tokio::test]
    async fn apply_diff_remove_nonexistent_is_ok() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let diff = ConfigDiff {
            removed: vec!["ghost".to_string()],
            ..empty_diff()
        };

        // Should not panic
        apply_diff(&diff, &registry, &tm, &inners, false, None).await;
    }

    #[tokio::test]
    async fn apply_diff_changed_shuts_down_old() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Change the endpoint config — create_adapter will fail to spawn a real process,
        // but the old adapter should be shut down and removed
        let changed_ep = EndpointConfig {
            name: "ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Stdio,
            command: Some("/nonexistent/binary/that/wont/start".to_string()),
            args: None,
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        };
        let diff = ConfigDiff {
            changed: vec![("ep".to_string(), changed_ep)],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners, false, None).await;

        // Old adapter should have been shut down
        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn apply_diff_added_with_invalid_command_registers_as_failed() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();

        let new_ep = EndpointConfig {
            name: "bad_ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Stdio,
            command: Some("/nonexistent/binary/that/wont/start".to_string()),
            args: None,
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners, false, None).await;

        // Wait for background initialization to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The failed adapter should appear in the registry but with unhealthy status
        // and empty tool catalog
        assert!(registry.merged_catalog().await.is_empty()); // no tools exposed
        let entries = registry.entries().read().await;
        assert_eq!(entries.len(), 1); // but endpoint IS registered
        let entry = entries.get("bad_ep").expect("bad_ep should be registered");
        assert!(matches!(entry.adapter.health(), HealthStatus::Unhealthy(_)));
    }

    #[tokio::test]
    async fn apply_diff_preserves_unchanged_endpoints() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let shutdown_keep = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_remove = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        registry
            .register(
                "keep".into(),
                Box::new(MockAdapter::healthy(
                    vec![make_tool("t1")],
                    shutdown_keep.clone(),
                )),
                "stdio".into(),
                None,
                Some("keep".into()),
            )
            .await;
        registry
            .register(
                "remove".into(),
                Box::new(MockAdapter::healthy(
                    vec![make_tool("t2")],
                    shutdown_remove.clone(),
                )),
                "stdio".into(),
                None,
                Some("remove".into()),
            )
            .await;

        let diff = ConfigDiff {
            removed: vec!["remove".to_string()],
            unchanged: vec!["keep".to_string()],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners, false, None).await;

        // "keep" should still be alive
        assert!(!shutdown_keep.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(registry.merged_catalog().await.len(), 1);
        // Single-server no-prefix mode: only one adapter remains, so no prefix
        assert_eq!(registry.merged_catalog().await[0].name, "t1");

        // "remove" should be shut down
        assert!(shutdown_remove.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn apply_diff_added_oauth_creates_oauth_adapter() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();

        let new_ep = EndpointConfig {
            name: "oauth_ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Oauth,
            command: None,
            args: None,
            url: Some("http://localhost:5000/mcp".to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: Some("https://auth.example.com".to_string()),
            client_id: Some("client123".to_string()),
            client_secret: None,
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners, false, None).await;

        // Wait for background initialization to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The OAuth adapter should be registered (not a FailedAdapter with restart message)
        let entries = registry.entries().read().await;
        assert_eq!(entries.len(), 1);
        let entry = entries
            .get("oauth_ep")
            .expect("oauth_ep should be registered");
        // OAuthAdapter reports Unhealthy("needs login") when no tokens are available,
        // NOT the FailedAdapter message about restart
        match &entry.adapter.health() {
            HealthStatus::Unhealthy(msg) => {
                assert!(
                    !msg.contains("restart"),
                    "Should be a real OAuthAdapter, not a FailedAdapter with restart message. Got: {}",
                    msg
                );
            }
            other => {
                // Stopped is also acceptable — OAuthAdapter initializes to Stopped then
                // transitions to Unhealthy("needs login") after initialize()
                assert!(
                    matches!(other, HealthStatus::Stopped),
                    "Expected Unhealthy or Stopped, got: {:?}",
                    other
                );
            }
        }

        // Verify the inner was inserted
        let inner_map = inners.read().await;
        assert!(
            inner_map.contains_key("oauth_ep"),
            "Inner should be registered for oauth_ep"
        );
    }

    #[tokio::test]
    async fn apply_diff_threads_allow_insecure_oauth_to_oauth_adapter_config() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();

        let new_ep = EndpointConfig {
            name: "oauth_insecure_ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Oauth,
            command: None,
            args: None,
            url: Some("http://127.0.0.1:5000/mcp".to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: Some("http://127.0.0.1:5001".to_string()),
            client_id: Some("client123".to_string()),
            client_secret: None,
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners, true, None).await;

        // Wait for background initialization to register the OAuth adapter inner.
        let stop = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if inners.read().await.contains_key("oauth_insecure_ep") {
                break;
            }
            if std::time::Instant::now() >= stop {
                panic!("OAuthAdapterInner was never registered");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let inner_map = inners.read().await;
        let inner = inner_map
            .get("oauth_insecure_ep")
            .expect("inner registered");
        assert!(
            inner.config.allow_insecure_oauth,
            "create_adapter must thread allow_insecure_oauth=true into OAuthAdapterConfig"
        );
    }

    // ---- G1: apply_diff_graceful direct coverage ------------------------

    #[tokio::test]
    async fn apply_diff_graceful_no_warnings_added_initializes_in_background() {
        // Drives apply_diff_graceful's added-endpoint branch with no warnings:
        // create_adapter fails fast (bad command) so the spawn replaces the
        // StartingAdapter with a FailedAdapter and runs rewire+invalidate.
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let new_ep = endpoint_with_bad_command("bad_added");
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff_graceful(
            &diff,
            &registry,
            &[],
            &Default::default(),
            &tm,
            &inners,
            false,
            None,
        )
        .await;

        // Wait for the background spawn to swap StartingAdapter for the
        // (Failed) adapter produced by create_adapter.
        let stop = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let entries = registry.entries().read().await;
            if let Some(entry) = entries.get("bad_added") {
                if !matches!(entry.adapter.health(), HealthStatus::Starting) {
                    break;
                }
            }
            drop(entries);
            if std::time::Instant::now() >= stop {
                panic!("background init never replaced StartingAdapter");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let entries = registry.entries().read().await;
        let entry = entries.get("bad_added").expect("bad_added registered");
        assert!(matches!(entry.adapter.health(), HealthStatus::Unhealthy(_)));
        // create_adapter failure path => empty cache, so invalidate ran cleanly.
        assert!(entry.tool_cache.read().await.is_none());
    }

    #[tokio::test]
    async fn apply_diff_graceful_added_warned_endpoint_registers_failed_adapter() {
        // Warned added endpoints must be registered as FailedAdapter immediately
        // (no background init, no create_adapter call).
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();

        let mut new_ep = endpoint_with_bad_command("warned_added");
        new_ep.disabled_tools = vec!["x".to_string()];
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };
        let warnings = vec![config::EndpointValidationWarning {
            endpoint_name: "warned_added".to_string(),
            message: "bogus command".to_string(),
        }];
        let warned: std::collections::HashSet<String> =
            warnings.iter().map(|w| w.endpoint_name.clone()).collect();

        apply_diff_graceful(
            &diff, &registry, &warnings, &warned, &tm, &inners, false, None,
        )
        .await;

        // No background spawn for warned endpoints — entry is final immediately.
        let entries = registry.entries().read().await;
        let entry = entries
            .get("warned_added")
            .expect("warned_added registered");
        match entry.adapter.health() {
            HealthStatus::Unhealthy(msg) => assert!(msg.contains("bogus command")),
            other => panic!("expected Unhealthy with warning message, got {:?}", other),
        }
        // disabled_tools preserved from the new endpoint config
        assert!(entry.disabled_tools.contains("x"));
    }

    #[tokio::test]
    async fn apply_diff_graceful_changed_warned_endpoint_registers_failed_adapter() {
        // Warned changed endpoints must shut down old adapter and register a
        // FailedAdapter immediately (no background init), preserving prior
        // disabled/disabled_tools state.
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        // Mark prior disabled state to verify it carries through the swap.
        {
            let mut entries = registry.entries().write().await;
            let entry = entries.get_mut("ep").unwrap();
            entry.disabled_tools.insert("preexisting".to_string());
        }

        let changed_ep = endpoint_with_bad_command("ep");
        let diff = ConfigDiff {
            changed: vec![("ep".to_string(), changed_ep)],
            ..empty_diff()
        };
        let warnings = vec![config::EndpointValidationWarning {
            endpoint_name: "ep".to_string(),
            message: "validation failed".to_string(),
        }];
        let warned: std::collections::HashSet<String> = ["ep".to_string()].into_iter().collect();

        apply_diff_graceful(
            &diff, &registry, &warnings, &warned, &tm, &inners, false, None,
        )
        .await;

        // Old adapter shut down synchronously.
        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
        let entries = registry.entries().read().await;
        let entry = entries.get("ep").expect("ep still registered");
        match entry.adapter.health() {
            HealthStatus::Unhealthy(msg) => assert!(msg.contains("validation failed")),
            other => panic!("expected Unhealthy with warning, got {:?}", other),
        }
        // Prior disabled_tools must be preserved across the warning swap.
        assert!(entry.disabled_tools.contains("preexisting"));
    }

    /// PR #69 audit gap 4: apply_diff_graceful must thread
    /// `allow_insecure_oauth` into the OAuth adapter config for added OAuth
    /// endpoints, exactly like `apply_diff` does. Without this, the watcher's
    /// graceful path could silently drop the flag and any subsequent
    /// loopback-targeting refresh would be rejected by the SSRF guard.
    #[tokio::test]
    async fn apply_diff_graceful_threads_allow_insecure_oauth_to_oauth_adapter_config() {
        let registry = Arc::new(AdapterRegistry::new());
        let (tm, inners) = test_oauth_infra();

        let new_ep = EndpointConfig {
            name: "oauth_graceful_insecure".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Oauth,
            command: None,
            args: None,
            url: Some("http://127.0.0.1:5000/mcp".to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: Some("http://127.0.0.1:5001".to_string()),
            client_id: Some("client123".to_string()),
            client_secret: None,
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };
        let warnings: Vec<config::EndpointValidationWarning> = vec![];
        let warned: std::collections::HashSet<String> = std::collections::HashSet::new();

        apply_diff_graceful(
            &diff, &registry, &warnings, &warned, &tm, &inners, true, None,
        )
        .await;

        // Wait for background initialization to register the OAuth adapter inner.
        let stop = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if inners.read().await.contains_key("oauth_graceful_insecure") {
                break;
            }
            if std::time::Instant::now() >= stop {
                panic!("OAuthAdapterInner was never registered");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let inner_map = inners.read().await;
        let inner = inner_map
            .get("oauth_graceful_insecure")
            .expect("inner registered");
        assert!(
            inner.config.allow_insecure_oauth,
            "apply_diff_graceful must thread allow_insecure_oauth=true into OAuthAdapterConfig"
        );
    }

    // ---- G2: registry rewires tools-changed listener after swap --------

    #[tokio::test]
    async fn registry_swap_rewires_tools_changed_listener() {
        // Mirrors apply_diff_graceful's spawn block: in-place swap of
        // entry.adapter, followed by rewire_tools_changed_listener +
        // invalidate_endpoint_tool_cache. After the swap, ticks on the NEW
        // adapter's sender must invalidate the per-endpoint cache; ticks on
        // the OLD adapter's sender must be ignored (its forwarder was aborted
        // and the only subscriber was dropped).
        let registry = Arc::new(AdapterRegistry::new());
        let (tx_old, _) = broadcast::channel::<()>(16);
        let (tx_new, _) = broadcast::channel::<()>(16);

        registry
            .register(
                "ep".into(),
                Box::new(NotifyingMockAdapter::new(
                    vec![make_tool("old_tool")],
                    tx_old.clone(),
                )),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Prime the per-endpoint cache so we can later detect invalidation.
        let _ = registry.merged_catalog().await;
        {
            let entries = registry.entries().read().await;
            let entry = entries.get("ep").unwrap();
            assert!(
                entry.tool_cache.read().await.is_some(),
                "cache should be primed before swap"
            );
        }

        // Simulate apply_diff_graceful's in-place swap + rewire + invalidate.
        {
            let mut entries = registry.entries().write().await;
            let entry = entries.get_mut("ep").unwrap();
            entry.adapter = Box::new(NotifyingMockAdapter::new(
                vec![make_tool("new_tool")],
                tx_new.clone(),
            ));
        }
        registry.rewire_tools_changed_listener("ep").await;
        registry.invalidate_endpoint_tool_cache("ep").await;

        // Re-prime the cache so the next tick has something to invalidate.
        let _ = registry.merged_catalog().await;
        {
            let entries = registry.entries().read().await;
            let entry = entries.get("ep").unwrap();
            assert!(
                entry.tool_cache.read().await.is_some(),
                "cache should be re-primed after swap"
            );
        }

        // Tick on NEW sender must propagate to the listener and clear cache.
        tx_new.send(()).expect("new send");
        let stop = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut cleared = false;
        while std::time::Instant::now() < stop {
            let entries = registry.entries().read().await;
            let entry = entries.get("ep").unwrap();
            if entry.tool_cache.read().await.is_none() {
                cleared = true;
                break;
            }
            drop(entries);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "listener was not rewired against the new adapter's sender"
        );

        // Re-prime once more, then fire on the OLD sender. The OLD listener
        // was aborted by rewire_tools_changed_listener; the per-endpoint
        // cache must remain intact.
        let _ = registry.merged_catalog().await;
        let _ = tx_old.send(()); // may be Err(_) if no subscribers — that's fine
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let entries = registry.entries().read().await;
        let entry = entries.get("ep").unwrap();
        assert!(
            entry.tool_cache.read().await.is_some(),
            "old adapter's sender must NOT invalidate the cache after rewire"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_oauth_client_creds tests (Wave 3a)
    // -----------------------------------------------------------------------

    fn oauth_endpoint(
        name: &str,
        client_id: Option<&str>,
        client_secret: Option<&str>,
    ) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Oauth,
            command: None,
            args: None,
            url: Some("https://mcp.example.com".to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: Some("https://auth.example.com".to_string()),
            client_id: client_id.map(|s| s.to_string()),
            client_secret: client_secret.map(|s| s.to_string()),
            scopes: None,
            token_endpoint: None,
            server_type_override: None,
        }
    }

    #[tokio::test]
    async fn adapter_init_prefers_dcr_file_over_config_secret() {
        use crate::token_manager::DcrCredentials;
        let tmp = tempfile::tempdir().unwrap();
        let tm = TokenManager::new(tmp.path().to_path_buf());
        tm.save_dcr(
            "ep",
            &DcrCredentials {
                client_id: "dcr-client".to_string(),
                client_secret: Some("dcr-secret".to_string()),
                client_secret_expires_at: 0,
                registered_at: 1_700_000_000,
            },
        )
        .await
        .unwrap();

        let ep = oauth_endpoint("ep", Some("toml-client"), Some("toml-secret"));
        let (id, secret) = resolve_oauth_client_creds(&ep, &tm).await;
        assert_eq!(id, "dcr-client");
        assert_eq!(secret.as_deref(), Some("dcr-secret"));
    }

    #[tokio::test]
    async fn adapter_init_falls_back_to_legacy_toml_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = TokenManager::new(tmp.path().to_path_buf());

        let ep = oauth_endpoint("ep", Some("toml-client"), Some("toml-secret"));
        let (id, secret) = resolve_oauth_client_creds(&ep, &tm).await;
        assert_eq!(id, "toml-client");
        assert_eq!(secret.as_deref(), Some("toml-secret"));
    }

    #[tokio::test]
    async fn adapter_init_no_dcr_no_secret_returns_config_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        let tm = TokenManager::new(tmp.path().to_path_buf());

        let ep = oauth_endpoint("ep", Some("toml-client"), None);
        let (id, secret) = resolve_oauth_client_creds(&ep, &tm).await;
        assert_eq!(id, "toml-client");
        assert!(secret.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_init_warns_when_falling_back_to_toml_secret() {
        use std::io;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);
        impl io::Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let tmp = tempfile::tempdir().unwrap();
        let tm = TokenManager::new(tmp.path().to_path_buf());
        let ep = oauth_endpoint("warned-ep", Some("toml-client"), Some("toml-secret"));
        let (id, secret) = resolve_oauth_client_creds(&ep, &tm).await;
        assert_eq!(id, "toml-client");
        assert_eq!(secret.as_deref(), Some("toml-secret"));

        let captured = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("legacy `client_secret`") && captured.contains("warned-ep"),
            "expected WARN log mentioning legacy client_secret and the endpoint name; got: {captured}"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_oauth_token_endpoint / conventional_token_endpoint tests
    // -----------------------------------------------------------------------

    /// Helper: build an OAuth endpoint with explicit `oauth_server_url` and
    /// optional explicit `token_endpoint`. Used by the resolver tests below.
    fn oauth_endpoint_with(
        name: &str,
        oauth_server_url: Option<&str>,
        token_endpoint: Option<&str>,
    ) -> EndpointConfig {
        EndpointConfig {
            name: name.to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Oauth,
            command: None,
            args: None,
            url: Some("https://mcp.example.com".to_string()),
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: oauth_server_url.map(|s| s.to_string()),
            client_id: Some("cid".to_string()),
            client_secret: None,
            scopes: None,
            token_endpoint: token_endpoint.map(|s| s.to_string()),
            server_type_override: None,
        }
    }

    /// `conventional_token_endpoint` must trim a single trailing slash on the
    /// base so that `https://accounts.google.com/` does NOT produce the
    /// malformed `https://accounts.google.com//token` URL that triggers the
    /// Google `invalid_request` refresh failures called out in the spec.
    #[test]
    fn conventional_token_endpoint_strips_trailing_slash() {
        assert_eq!(
            conventional_token_endpoint("https://accounts.google.com/"),
            "https://accounts.google.com/token"
        );
        assert_eq!(
            conventional_token_endpoint("https://accounts.google.com"),
            "https://accounts.google.com/token"
        );
        // Multiple trailing slashes also collapse (defense-in-depth).
        assert_eq!(
            conventional_token_endpoint("https://example.com///"),
            "https://example.com/token"
        );
    }

    /// Explicit `ep.token_endpoint` always wins — discovery must NOT run when
    /// the operator has configured the endpoint URL directly.
    #[tokio::test]
    async fn resolve_oauth_token_endpoint_prefers_explicit() {
        let ep = oauth_endpoint_with(
            "ep",
            Some("https://accounts.google.com/"),
            Some("https://oauth2.googleapis.com/token"),
        );
        let resolved = resolve_oauth_token_endpoint(&ep, false).await;
        assert_eq!(resolved, "https://oauth2.googleapis.com/token");
    }

    /// Whitespace-only `token_endpoint` is treated as "not set" so we fall
    /// through to discovery / convention rather than POSTing to an empty URL.
    #[tokio::test]
    async fn resolve_oauth_token_endpoint_treats_blank_explicit_as_unset() {
        let ep = oauth_endpoint_with("ep", Some("https://accounts.google.com/"), Some("   "));
        // Discovery against accounts.google.com would hit the network; use a
        // loopback URL instead which the SSRF guard rejects fast so we reach
        // the conventional fallback deterministically. Replace oauth_server_url.
        let mut ep = ep;
        ep.oauth_server_url = Some("http://127.0.0.1:1/".to_string());
        let resolved = resolve_oauth_token_endpoint(&ep, false).await;
        assert_eq!(resolved, "http://127.0.0.1:1/token");
    }

    /// When discovery fails (here: SSRF guard rejects loopback with
    /// `allow_insecure=false`), the resolver falls back to
    /// `conventional_token_endpoint(oauth_server_url)` and the result must NOT
    /// contain a `//token` segment — this is the regression the spec calls
    /// out for Google Drive's trailing-slash `oauth_server_url`.
    #[tokio::test]
    async fn resolve_oauth_token_endpoint_falls_back_slash_safely_on_discovery_failure() {
        let ep = oauth_endpoint_with("ep", Some("http://127.0.0.1:1/"), None);
        let resolved = resolve_oauth_token_endpoint(&ep, false).await;
        assert_eq!(resolved, "http://127.0.0.1:1/token");
        assert!(
            !resolved.contains("//token"),
            "fallback must not produce //token; got: {resolved}"
        );
    }

    /// Missing `oauth_server_url` AND missing `token_endpoint` produces
    /// `"/token"` — preserving the prior empty-base behaviour so existing
    /// misconfigured endpoints fail the same way they did before this change
    /// (rather than panicking or hanging on discovery).
    #[tokio::test]
    async fn resolve_oauth_token_endpoint_empty_base_returns_slash_token() {
        let ep = oauth_endpoint_with("ep", None, None);
        let resolved = resolve_oauth_token_endpoint(&ep, false).await;
        assert_eq!(resolved, "/token");
    }

    // ---- R4.B: hot reload of ProfileRegistry ----------------------------
    //
    // The matrix rows under §11 (#18 add, #19 remove, #20 invalid keep-last-
    // good) are exercised by `reload_and_apply`: it is the extracted body of
    // the watcher loop, so driving it directly with on-disk config files
    // mirrors the watcher tick without the filesystem-event timing flakiness.
    mod hot_reload {
        mod profile {
            use super::super::*;
            use crate::config::Config;
            use crate::profile_registry::ProfileRegistry;
            use std::path::PathBuf;
            use std::sync::atomic::AtomicBool;
            use std::sync::Arc;
            use tokio::sync::RwLock;

            /// Build the initial in-memory state the watcher would have after
            /// loading `config.toml` for the first time at startup.
            async fn setup_initial(
                initial_toml: &str,
            ) -> (
                tempfile::TempDir,
                PathBuf,
                Arc<RwLock<Config>>,
                Arc<AdapterRegistry>,
                Arc<ProfileRegistry>,
                Arc<AtomicBool>,
                Arc<TokenManager>,
                OAuthAdapterInners,
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let path = tmp.path().join("config.toml");
                std::fs::write(&path, initial_toml).unwrap();
                let (initial, _warnings) = config::load_config_graceful(&path).unwrap();

                let registry = Arc::new(AdapterRegistry::new());
                let profile_registry = Arc::new(ProfileRegistry::new((*registry).clone()));
                // Mirror main.rs: rebuild once at startup against the initial
                // config so the watcher's first reload sees a populated map.
                profile_registry
                    .rebuild(initial.profiles.as_deref().unwrap_or(&[]))
                    .await;

                let current_config = Arc::new(RwLock::new(initial));
                let js_mode = Arc::new(AtomicBool::new(false));
                let (token_manager, inners) = test_oauth_infra();
                (
                    tmp,
                    path,
                    current_config,
                    registry,
                    profile_registry,
                    js_mode,
                    token_manager,
                    inners,
                )
            }

            const CONFIG_NO_PROFILES: &str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "ep1"
transport = "stdio"
command = "/bin/true"
"#;

            const CONFIG_ONE_PROFILE: &str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "ep1"
transport = "stdio"
command = "/bin/true"

[[profiles]]
name = "Work"
path = "work"
endpoints = ["ep1"]
js_execution = false
toon_output = true
"#;

            /// Matrix #18: adding a `[[profiles]]` block to `config.toml`
            /// makes the new profile discoverable on the next watcher tick.
            #[tokio::test]
            async fn add_profile_appears_in_registry_after_reload() {
                let (_tmp, path, current_config, registry, profile_registry, js_mode, tm, inners) =
                    setup_initial(CONFIG_NO_PROFILES).await;

                assert!(profile_registry.get("work").await.is_none());

                std::fs::write(&path, CONFIG_ONE_PROFILE).unwrap();
                reload_and_apply(
                    &path,
                    &current_config,
                    &registry,
                    &js_mode,
                    &profile_registry,
                    &tm,
                    &inners,
                    None,
                )
                .await
                .expect("reload should succeed");

                let ctx = profile_registry
                    .get("work")
                    .await
                    .expect("profile 'work' should be served after reload");
                assert_eq!(ctx.config.name, "Work");
                assert_eq!(ctx.config.endpoints, vec!["ep1".to_string()]);
                assert_eq!(
                    current_config
                        .read()
                        .await
                        .profiles
                        .as_deref()
                        .unwrap()
                        .len(),
                    1
                );
            }

            /// Matrix #19: removing a `[[profiles]]` block from
            /// `config.toml` makes the prior profile vanish from the
            /// registry on the next watcher tick (its `/mcp/{path}` route
            /// then 404s via `ProfileRegistry::get` returning `None`).
            #[tokio::test]
            async fn remove_profile_disappears_from_registry_after_reload() {
                let (_tmp, path, current_config, registry, profile_registry, js_mode, tm, inners) =
                    setup_initial(CONFIG_ONE_PROFILE).await;

                assert!(profile_registry.get("work").await.is_some());

                std::fs::write(&path, CONFIG_NO_PROFILES).unwrap();
                reload_and_apply(
                    &path,
                    &current_config,
                    &registry,
                    &js_mode,
                    &profile_registry,
                    &tm,
                    &inners,
                    None,
                )
                .await
                .expect("reload should succeed");

                assert!(
                    profile_registry.get("work").await.is_none(),
                    "removed profile must no longer be served"
                );
                assert!(profile_registry.list().await.is_empty());
            }

            /// Matrix #20: an updated `config.toml` whose `[[profiles]]`
            /// block fails fail-fast validation (here: references an
            /// undeclared endpoint) is rejected. `reload_and_apply` returns
            /// `Err` and the previously-served profile registry is kept
            /// intact — keep-last-good semantics per spec §11.
            #[tokio::test]
            async fn invalid_profile_reload_keeps_last_good_registry() {
                let (_tmp, path, current_config, registry, profile_registry, js_mode, tm, inners) =
                    setup_initial(CONFIG_ONE_PROFILE).await;

                let good_before = profile_registry
                    .get("work")
                    .await
                    .expect("baseline profile must be served");
                assert_eq!(good_before.config.endpoints, vec!["ep1".to_string()]);

                // New config references an endpoint that does not exist —
                // `validate_profiles` returns a ValidationError, so
                // `load_config_graceful` (and therefore `reload_and_apply`)
                // bails out without touching the registry.
                let invalid = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "ep1"
transport = "stdio"
command = "/bin/true"

[[profiles]]
name = "Broken"
path = "broken"
endpoints = ["does-not-exist"]
js_execution = false
toon_output = true
"#;
                std::fs::write(&path, invalid).unwrap();

                let err = reload_and_apply(
                    &path,
                    &current_config,
                    &registry,
                    &js_mode,
                    &profile_registry,
                    &tm,
                    &inners,
                    None,
                )
                .await
                .expect_err("invalid profile config must be rejected");
                assert!(
                    matches!(err, config::ConfigError::ValidationError(_)),
                    "expected ValidationError, got {err:?}"
                );

                // The previously-served profile is still reachable, the
                // broken one was never installed, and the baseline config
                // snapshot was not advanced.
                let good_after = profile_registry
                    .get("work")
                    .await
                    .expect("last-good profile must remain after failed reload");
                assert_eq!(good_after.config.endpoints, vec!["ep1".to_string()]);
                assert!(
                    profile_registry.get("broken").await.is_none(),
                    "broken profile must never be installed"
                );
                let baseline = current_config.read().await;
                let baseline_names: Vec<String> = baseline
                    .profiles
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|p| p.name.clone())
                    .collect();
                assert_eq!(baseline_names, vec!["Work".to_string()]);
            }
        }

        // ---- R3.D: tools_changed broadcast coverage from reload_and_apply ----
        //
        // Acceptance #4 in the R3.D spec: a single `reload_and_apply` that
        // adds one endpoint, removes another, and leaves a third unchanged
        // must emit exactly one `tools_changed` tick for the added endpoint,
        // exactly one for the removed endpoint, and zero for the unchanged
        // one. All add/remove paths in `apply_diff_graceful` flow through
        // `AdapterRegistry::register` / `remove`, which already tick once
        // per call (registry.rs:195 and :206); this test pins that wiring
        // end-to-end so future watcher refactors can't silently break it.
        mod tools_changed {
            use super::super::*;
            use crate::config::Config;
            use crate::profile_registry::ProfileRegistry;
            use std::collections::HashMap;
            use std::path::PathBuf;
            use std::sync::atomic::AtomicBool;
            use std::sync::Arc;
            use tokio::sync::RwLock;

            /// Initial config: `keep` (unchanged across reload) + `remove_me`
            /// (removed on reload).
            const CONFIG_BEFORE: &str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "keep"
transport = "stdio"
command = "/bin/true"

[[endpoints]]
name = "remove_me"
transport = "stdio"
command = "/bin/true"
"#;

            /// New config: `keep` (unchanged) + `add_me` (newly added).
            const CONFIG_AFTER: &str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "keep"
transport = "stdio"
command = "/bin/true"

[[endpoints]]
name = "add_me"
transport = "stdio"
command = "/bin/true"
"#;

            /// Build the in-memory state matching what main.rs would have
            /// after loading `CONFIG_BEFORE` at startup, with `keep` and
            /// `remove_me` pre-registered in the adapter registry so the
            /// reload's `removed` branch actually has something to remove
            /// (and therefore actually ticks).
            async fn setup_with_endpoints() -> (
                tempfile::TempDir,
                PathBuf,
                Arc<RwLock<Config>>,
                Arc<AdapterRegistry>,
                Arc<ProfileRegistry>,
                Arc<AtomicBool>,
                Arc<TokenManager>,
                OAuthAdapterInners,
            ) {
                let tmp = tempfile::tempdir().unwrap();
                let path = tmp.path().join("config.toml");
                std::fs::write(&path, CONFIG_BEFORE).unwrap();
                let (initial, _warnings) = config::load_config_graceful(&path).unwrap();

                let registry = Arc::new(AdapterRegistry::new());
                // Pre-register the two initial endpoints as MockAdapters so
                // the reload's `remove("remove_me")` call actually finds and
                // removes an entry (and therefore ticks). `keep` is also
                // registered so its presence in the unchanged branch
                // exercises the "no-op for unchanged" path realistically.
                let dummy = Arc::new(std::sync::atomic::AtomicBool::new(false));
                for name in ["keep", "remove_me"] {
                    registry
                        .register(
                            name.into(),
                            Box::new(MockAdapter::healthy(vec![make_tool("t")], dummy.clone())),
                            "stdio".into(),
                            None,
                            Some(name.into()),
                        )
                        .await;
                }

                let profile_registry = Arc::new(ProfileRegistry::new((*registry).clone()));
                profile_registry
                    .rebuild(initial.profiles.as_deref().unwrap_or(&[]))
                    .await;

                let current_config = Arc::new(RwLock::new(initial));
                let js_mode = Arc::new(AtomicBool::new(false));
                let (token_manager, inners) = test_oauth_infra();
                (
                    tmp,
                    path,
                    current_config,
                    registry,
                    profile_registry,
                    js_mode,
                    token_manager,
                    inners,
                )
            }

            /// Drain every tick the subscriber sees within `budget`, returning
            /// a per-endpoint count map. Uses a per-recv timeout (instead of
            /// `try_recv`) so the test stays robust against background-task
            /// scheduling jitter — the budget is generous enough to absorb
            /// any synchronously-emitted tick from `reload_and_apply` while
            /// still being short on test wall-clock.
            async fn drain_ticks(
                rx: &mut tokio::sync::broadcast::Receiver<String>,
                budget: std::time::Duration,
            ) -> HashMap<String, usize> {
                let deadline = std::time::Instant::now() + budget;
                let mut counts: HashMap<String, usize> = HashMap::new();
                loop {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let remaining = deadline - now;
                    match tokio::time::timeout(remaining, rx.recv()).await {
                        Ok(Ok(name)) => *counts.entry(name).or_insert(0) += 1,
                        Ok(Err(_)) => break, // Lagged or Closed — stop draining.
                        Err(_) => break,     // Timeout — no more ticks.
                    }
                }
                counts
            }

            /// Spec R3.D acceptance #4: a reload that adds one endpoint,
            /// removes another, and leaves a third unchanged must produce
            /// exactly one `tools_changed` tick for the added endpoint, one
            /// for the removed endpoint, and zero for the unchanged one.
            #[tokio::test]
            async fn reload_and_apply_ticks_added_and_removed_not_unchanged() {
                let (_tmp, path, current_config, registry, profile_registry, js_mode, tm, inners) =
                    setup_with_endpoints().await;

                // Subscribe AFTER the initial pre-registration so we only
                // observe the ticks driven by `reload_and_apply` itself.
                let mut rx = registry.subscribe_tools_changed();

                std::fs::write(&path, CONFIG_AFTER).unwrap();
                reload_and_apply(
                    &path,
                    &current_config,
                    &registry,
                    &js_mode,
                    &profile_registry,
                    &tm,
                    &inners,
                    None,
                )
                .await
                .expect("reload should succeed");

                let counts = drain_ticks(&mut rx, std::time::Duration::from_millis(150)).await;

                assert_eq!(
                    counts.get("add_me").copied().unwrap_or(0),
                    1,
                    "added endpoint must emit exactly one tick (counts={counts:?})"
                );
                assert_eq!(
                    counts.get("remove_me").copied().unwrap_or(0),
                    1,
                    "removed endpoint must emit exactly one tick (counts={counts:?})"
                );
                assert_eq!(
                    counts.get("keep").copied().unwrap_or(0),
                    0,
                    "unchanged endpoint must emit zero ticks (counts={counts:?})"
                );
                assert_eq!(
                    counts.values().sum::<usize>(),
                    2,
                    "reload should emit exactly 2 ticks total (counts={counts:?})"
                );
            }
        }

        /// Regression coverage for the desktop "Failed to load profiles"
        /// bug: after a new endpoint lands on disk via the same code path
        /// `oauth_setup_commit` uses (TOML writeback → watcher reload), the
        /// management API's `GET /api/endpoints/{name}/profiles` route must
        /// switch from 404 to 200. The bug was that the watcher updated its
        /// own internal `current_config` and the adapter registry, but the
        /// `ManagementState::config` Arc it pulled from at request time
        /// stayed stale — so the Profiles tab opened on a freshly-added
        /// OAuth MCP server surfaced "Failed to load profiles" forever.
        ///
        /// The fix wires the same `Arc<RwLock<Config>>` through both
        /// `ManagementState` and `reload_and_apply`, which this test
        /// asserts by driving the reload helper directly and oneshot-ing
        /// the membership route against the shared state.
        mod endpoint_membership {
            use super::super::*;
            use crate::management::{management_routes, ManagementState};
            use crate::profile_registry::ProfileRegistry;
            use axum::body::Body;
            use axum::http::{Request, StatusCode};
            use std::sync::atomic::AtomicBool;
            use std::sync::Arc;
            use std::time::Instant;
            use tokio::sync::RwLock;
            use tower::ServiceExt;

            const CONFIG_EMPTY: &str = r#"
[relay]
machine_name = "test"
"#;

            const CONFIG_WITH_NEW_ENDPOINT: &str = r#"
[relay]
machine_name = "test"

[[endpoints]]
name = "newserver"
transport = "stdio"
command = "/bin/true"
"#;

            #[tokio::test]
            async fn endpoint_profile_membership_visible_after_watcher_reload() {
                let tmp = tempfile::tempdir().unwrap();
                let path = tmp.path().join("config.toml");
                std::fs::write(&path, CONFIG_EMPTY).unwrap();
                let (initial, _warnings) = config::load_config_graceful(&path).unwrap();

                let registry = Arc::new(AdapterRegistry::new());
                let profile_registry = Arc::new(ProfileRegistry::new((*registry).clone()));
                profile_registry
                    .rebuild(initial.profiles.as_deref().unwrap_or(&[]))
                    .await;
                let shared_config = Arc::new(RwLock::new(initial));
                let js_mode = Arc::new(AtomicBool::new(false));
                let (token_manager, inners) = test_oauth_infra();

                // Same Arc on both sides — this is the wiring the bug
                // regressed: before the fix the watcher swapped a private
                // copy and `ManagementState` held a separate stale Arc.
                let state = ManagementState {
                    registry: registry.clone(),
                    config: shared_config.clone(),
                    start_time: Instant::now(),
                    config_path: Some(path.clone()),
                    oauth_flow_manager: None,
                    relay_port: 0,
                    oauth_adapter_inners: Some(inners.clone()),
                    token_manager: Some(token_manager.clone()),
                    setup_manager: None,
                    profile_registry: Some(profile_registry.clone()),
                    event_bus: None,
                };
                let app = management_routes(state);

                // Fixture sanity: the endpoint doesn't exist yet, so the
                // membership route must 404 — confirms the assertion
                // pivots on the reload, not on the route always returning
                // 200.
                let resp = app
                    .clone()
                    .oneshot(
                        Request::get("/api/endpoints/newserver/profiles")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    resp.status(),
                    StatusCode::NOT_FOUND,
                    "fixture sanity: 'newserver' must not exist before reload"
                );

                // Same code path `oauth_setup_commit` exercises: edit the
                // TOML file and let the watcher's reload helper reconcile
                // both the registry and the shared config snapshot.
                std::fs::write(&path, CONFIG_WITH_NEW_ENDPOINT).unwrap();
                reload_and_apply(
                    &path,
                    &shared_config,
                    &registry,
                    &js_mode,
                    &profile_registry,
                    &token_manager,
                    &inners,
                    None,
                )
                .await
                .expect("reload of valid config must succeed");

                let resp = app
                    .oneshot(
                        Request::get("/api/endpoints/newserver/profiles")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    resp.status(),
                    StatusCode::OK,
                    "after reload, membership route must see the new endpoint \
                     via the shared ManagementState::config Arc"
                );
                let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    body,
                    serde_json::json!({ "profiles": [] }),
                    "brand-new endpoint with no profiles should report an empty list"
                );
            }
        }
    }
}
