mod config;
mod js_sandbox;
mod management;
mod management_listener;
mod oauth;
mod token_manager;
mod watcher;

mod adapter;
mod advertise;
mod jsonrpc;
mod prefix;
mod registry;
mod server;
mod shell_env;
mod toon_convert;

use adapter::oauth::{OAuthAdapter, OAuthAdapterConfig, OAuthAdapterInner};
use adapter::{FailedAdapter, McpAdapter, StartingAdapter};
use clap::{Parser, Subcommand, ValueEnum};
use js_sandbox::MetaToolHandler;
use oauth::{OAuthFlowManager, OAuthSetupManager};
use registry::AdapterRegistry;
use server::{build_router, start_server, AppState};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use token_manager::TokenManager;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Per-endpoint shared OAuth adapter inner states, keyed by endpoint name.
/// Used by the callback handler to apply tokens to the correct adapter.
pub type OAuthAdapterInners = Arc<RwLock<HashMap<String, Arc<OAuthAdapterInner>>>>;
use watcher::ConfigWatcher;

#[derive(Parser)]
#[command(name = "endara-relay", version, about = "Endara Relay agent")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the relay agent
    Start {
        /// Base data directory for config, logs, and tokens
        #[arg(long, default_value = "~/.endara")]
        data_dir: String,

        /// Path to TOML configuration file (overrides <data-dir>/config.toml)
        #[arg(long)]
        config: Option<PathBuf>,

        /// Port to listen on
        #[arg(long, default_value = "9400")]
        port: u16,

        /// Log output format
        #[arg(long, default_value = "compact", value_parser = ["text", "compact", "json"])]
        log_format: String,

        /// Colorize stdout logs (auto detects TTY)
        #[arg(long, default_value = "auto", value_enum)]
        color: ColorMode,

        /// EnvFilter directive for the file log layer (default: debug,endara_relay=trace)
        #[arg(long)]
        file_log_level: Option<String>,

        /// Disable TOON (Token-Oriented Object Notation) conversion of JSON
        /// tool responses. Overrides `relay.toon_output` from config.toml.
        /// When unset, TOON conversion defaults to on.
        #[arg(long, default_value_t = false)]
        no_toon: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

/// Expand a path string, replacing a leading `~` with the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    }
}

fn init_tracing(
    color_mode: ColorMode,
    log_format: &str,
    file_log_level: Option<String>,
    log_dir: &std::path::Path,
) {
    use std::io::IsTerminal;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, Layer};

    let use_color = match color_mode {
        ColorMode::Auto => std::io::stdout().is_terminal(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    };

    // Stdout filter: RUST_LOG or default. Independent of the file filter so the
    // two layers can run at different verbosity levels.
    let stdout_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,endara_relay=debug"));
    let file_filter = EnvFilter::new(
        file_log_level
            .as_deref()
            .unwrap_or("debug,endara_relay=trace"),
    );

    let file_appender = tracing_appender::rolling::daily(log_dir, "relay.log");
    let file_layer = fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_filter(file_filter)
        .boxed();

    let stdout_layer = match log_format {
        "json" => fmt::layer()
            .json()
            .with_ansi(use_color)
            .with_filter(stdout_filter)
            .boxed(),
        "compact" => fmt::layer()
            .compact()
            .with_ansi(use_color)
            .with_filter(stdout_filter)
            .boxed(),
        _ => fmt::layer()
            .with_ansi(use_color)
            .with_filter(stdout_filter)
            .boxed(),
    };

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            data_dir,
            config,
            port,
            log_format,
            color,
            file_log_level,
            no_toon,
        } => {
            let data_dir_path = expand_tilde(&data_dir);
            let log_dir = data_dir_path.join("logs");
            let config_explicit = config.is_some();
            let config_path = config.unwrap_or_else(|| data_dir_path.join("config.toml"));

            init_tracing(color, &log_format, file_log_level, &log_dir);
            info!(config = %config_path.display(), data_dir = %data_dir_path.display(), "Starting endara-relay");

            // First-run config copy: when using a non-default data dir without an
            // explicit --config, copy the production config so dev instances inherit
            // the same endpoint setup.
            let default_data_dir = dirs::home_dir()
                .map(|h| h.join(".endara"))
                .unwrap_or_default();
            if !config_explicit && data_dir_path != default_data_dir {
                let resolved_config = config::expand_tilde(&config_path);
                if !resolved_config.exists() {
                    let production_config = default_data_dir.join("config.toml");
                    if production_config.exists() {
                        // Ensure the data dir and standard subdirs exist
                        if let Err(e) = std::fs::create_dir_all(&data_dir_path) {
                            warn!(error = %e, path = %data_dir_path.display(), "Failed to create data directory");
                        }
                        for sub in &["logs", "tokens"] {
                            let sub_path = data_dir_path.join(sub);
                            if let Err(e) = std::fs::create_dir_all(&sub_path) {
                                warn!(error = %e, path = %sub_path.display(), "Failed to create subdirectory");
                            }
                        }
                        match std::fs::copy(&production_config, &resolved_config) {
                            Ok(_) => {
                                info!(
                                    src = %production_config.display(),
                                    dst = %resolved_config.display(),
                                    "Copied production config to dev data directory"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    src = %production_config.display(),
                                    dst = %resolved_config.display(),
                                    "Failed to copy production config"
                                );
                            }
                        }
                    }
                }
            }

            let (cfg, validation_warnings) = match config::load_config_graceful(&config_path) {
                Ok((cfg, warnings)) => {
                    info!(
                        machine_name = %cfg.relay.machine_name,
                        endpoints = cfg.endpoints.len(),
                        warnings = warnings.len(),
                        "Configuration loaded successfully"
                    );
                    for w in &warnings {
                        warn!("{}", w);
                    }
                    (cfg, warnings)
                }
                Err(config::ConfigError::IoError(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::NotFound =>
                {
                    info!(
                        path = %config_path.display(),
                        "Config file not found, creating default configuration"
                    );
                    match config::create_default_config_file(&config_path) {
                        Ok(cfg) => {
                            info!(
                                machine_name = %cfg.relay.machine_name,
                                path = %config_path.display(),
                                "Created default configuration"
                            );
                            (cfg, Vec::new())
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to create default configuration");
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to load configuration");
                    std::process::exit(1);
                }
            };

            // Collect endpoint names that have validation warnings
            let warned_names = config::warned_endpoint_names(&validation_warnings);
            // Build a map from endpoint name to its warning message(s)
            let warning_messages: std::collections::HashMap<String, String> = {
                let mut map = std::collections::HashMap::new();
                for w in &validation_warnings {
                    map.entry(w.endpoint_name.clone())
                        .and_modify(|msg: &mut String| {
                            msg.push_str("; ");
                            msg.push_str(&w.message);
                        })
                        .or_insert_with(|| w.message.clone());
                }
                map
            };

            // Initialize OAuth infrastructure
            // ENDARA_TOKEN_DIR env var allows integration tests to isolate token storage.
            let token_dir_path = std::env::var("ENDARA_TOKEN_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| data_dir_path.join("tokens"));
            let token_dir =
                match endara_relay::token_security::ensure_token_dir_secure(&token_dir_path) {
                    Ok(path) => path,
                    Err(e) => {
                        error!(error = %e, "Failed to secure token directory");
                        token_dir_path // Fall back to unsecured path
                    }
                };
            // Warn (but do not block) if token_dir lives inside a known
            // consumer cloud-sync provider — refresh tokens stored there
            // get uploaded off-device.
            endara_relay::token_security::warn_if_cloud_synced(&token_dir);
            let token_manager = Arc::new(TokenManager::new(token_dir));
            let oauth_flow_manager = Arc::new(OAuthFlowManager::new());
            let oauth_adapter_inners: OAuthAdapterInners = Arc::new(RwLock::new(HashMap::new()));

            // Create adapter registry
            let registry = AdapterRegistry::new();

            // Track duplicate endpoint names: first occurrence wins
            let mut registered_names = std::collections::HashSet::new();

            // Collect endpoints that need background initialization
            let mut deferred_init: Vec<config::EndpointConfig> = Vec::new();
            // OAuth endpoints constructed synchronously but whose `initialize()`
            // must run in the background. We keep the built `OAuthAdapter` here
            // (out of the registry) and swap it in once `initialize` completes,
            // while a `StartingAdapter` placeholder occupies the registry slot.
            // The optional `discovery` field `(base_url, allow_insecure)` tells
            // the spawn task to run RFC 8414 discovery against `base_url` and
            // patch the adapter's token endpoint via `set_token_endpoint_override`
            // before `initialize()` — keeping the potentially-slow HTTP call
            // off the main per-endpoint loop.
            struct DeferredOAuthInit {
                name: String,
                adapter: OAuthAdapter,
                discovery: Option<(String, bool)>,
            }
            let mut deferred_oauth_init: Vec<DeferredOAuthInit> = Vec::new();

            // Register adapters for each endpoint (non-blocking)
            for ep in &cfg.endpoints {
                // Handle duplicate endpoint names: first wins, rest skipped
                if !ep.name.is_empty() && !registered_names.insert(ep.name.clone()) {
                    let msg = format!("Duplicate endpoint name: '{}'", ep.name);
                    warn!(endpoint = %ep.name, "{}", msg);
                    continue;
                }

                // If this endpoint has validation warnings, register as FailedAdapter
                if warned_names.contains(&ep.name) {
                    let msg = warning_messages.get(&ep.name).cloned().unwrap_or_default();
                    warn!(endpoint = %ep.name, "Registering as failed due to validation error: {}", msg);
                    let adapter: Box<dyn McpAdapter> = Box::new(
                        FailedAdapter::new(msg)
                            .with_server_type_override(ep.server_type_override.clone()),
                    );
                    registry
                        .register(
                            ep.name.clone(),
                            adapter,
                            ep.transport.to_string(),
                            ep.description.clone(),
                            ep.resolved_tool_prefix(),
                        )
                        .await;
                    continue;
                }

                info!(name = %ep.name, transport = %ep.transport, "Configuring endpoint");

                // OAuth endpoints: construct synchronously (so `shared_inner`
                // is in `oauth_adapter_inners` before the management listener
                // binds), register a `StartingAdapter` placeholder, and defer
                // the slow `initialize().await` to a background task.
                //
                // Resolution of the OAuth token endpoint is **deliberately
                // synchronous-only here**: we use the explicit `token_endpoint`
                // when set, otherwise the conventional `{oauth_server_url}/token`
                // fallback. The full RFC 8414 discovery (which can do unbounded
                // network I/O against an unreachable `oauth_server_url`) is
                // moved into the OAuth spawn task below and applied via
                // `set_token_endpoint_override`. This keeps the per-endpoint
                // loop O(file-read) per OAuth endpoint so a single unreachable
                // upstream cannot stall every other endpoint's registration.
                if ep.transport == config::Transport::Oauth {
                    let allow_insecure_oauth = cfg.relay.allow_insecure_oauth.unwrap_or(false);
                    let (client_id, client_secret) =
                        watcher::resolve_oauth_client_creds(ep, token_manager.as_ref()).await;

                    let explicit_token_endpoint = ep
                        .token_endpoint
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let oauth_base = ep
                        .oauth_server_url
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    let initial_token_endpoint =
                        explicit_token_endpoint.clone().unwrap_or_else(|| {
                            watcher::conventional_token_endpoint(
                                oauth_base.as_deref().unwrap_or(""),
                            )
                        });
                    let discovery_params = match (&explicit_token_endpoint, &oauth_base) {
                        (None, Some(base)) => Some((base.clone(), allow_insecure_oauth)),
                        _ => None,
                    };

                    let oauth_config = OAuthAdapterConfig {
                        endpoint_name: ep.name.clone(),
                        url: ep.url.clone().unwrap_or_default(),
                        token_endpoint_url: initial_token_endpoint,
                        client_id,
                        client_secret,
                        heartbeat_interval_secs: 30,
                        probe_timeout_secs: 10,
                        probe_failure_threshold: 3,
                        server_type_override: ep.server_type_override.clone(),
                        // Mirror the global SSRF posture for the refresh-time
                        // discovery fallback so operators who already opted into
                        // `relay.allow_insecure_oauth` for the initial discovery
                        // see consistent behavior on rediscovery.
                        allow_insecure_oauth,
                    };

                    let adapter = OAuthAdapter::new(oauth_config, token_manager.clone());
                    let shared_inner = adapter.shared_inner();
                    oauth_adapter_inners
                        .write()
                        .await
                        .insert(ep.name.clone(), shared_inner);

                    registry
                        .register(
                            ep.name.clone(),
                            Box::new(StartingAdapter),
                            ep.transport.to_string(),
                            ep.description.clone(),
                            ep.resolved_tool_prefix(),
                        )
                        .await;
                    deferred_oauth_init.push(DeferredOAuthInit {
                        name: ep.name.clone(),
                        adapter,
                        discovery: discovery_params,
                    });
                    continue;
                }

                // Register with Starting status immediately; initialize in background later
                registry
                    .register(
                        ep.name.clone(),
                        Box::new(StartingAdapter),
                        ep.transport.to_string(),
                        ep.description.clone(),
                        ep.resolved_tool_prefix(),
                    )
                    .await;
                deferred_init.push(ep.clone());
            }

            // Apply disabled state from config
            for ep in &cfg.endpoints {
                if ep.disabled {
                    let mut entries = registry.entries().write().await;
                    if let Some(entry) = entries.get_mut(&ep.name) {
                        entry.disabled = true;
                    }
                }
                if !ep.disabled_tools.is_empty() {
                    let mut entries = registry.entries().write().await;
                    if let Some(entry) = entries.get_mut(&ep.name) {
                        entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
                    }
                }
            }

            // Build and start HTTP server
            let registry = Arc::new(registry);

            // Spawn background initialization for every endpoint and collect
            // their JoinHandles so `main()` can wait for them to settle (or
            // give up after `startup_init_timeout_secs`) before binding the
            // MCP TCP listener. The handles are detached if the wait times
            // out — dropping a tokio JoinHandle does not abort the task, so
            // late-arriving adapters keep initializing and publish their
            // tools to the registry via the existing `invalidate_catalog_cache`
            // tail.
            let settled_inits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut init_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
            let allow_insecure_oauth = cfg.relay.allow_insecure_oauth.unwrap_or(false);
            for ep in deferred_init {
                let reg = registry.clone();
                let tm = token_manager.clone();
                let oai = oauth_adapter_inners.clone();
                let settled = settled_inits.clone();
                let handle = tokio::spawn(async move {
                    let adapter =
                        watcher::create_adapter(&ep, &tm, &oai, allow_insecure_oauth).await;
                    let mut entries = reg.entries().write().await;
                    if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                        entry.adapter = adapter;
                        if entry.disabled {
                            let _ = entry.adapter.shutdown().await;
                        }
                    }
                    drop(entries);
                    reg.rewire_tools_changed_listener(&ep.name).await;
                    reg.invalidate_catalog_cache().await;
                    info!(endpoint = %ep.name, "Adapter initialized");
                    settled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                });
                init_handles.push(handle);
            }

            // Spawn background initialization for OAuth endpoints. Their
            // `shared_inner` is already in `oauth_adapter_inners` so the
            // callback handler and "Reauthenticate" flow work immediately;
            // the slow `initialize().await` (apply_tokens → inner HttpAdapter
            // → MCP handshake) runs here and swaps the real adapter in.
            //
            // RFC 8414 discovery against `oauth_server_url` also happens here
            // (not on the main task), so an unreachable upstream OAuth server
            // cannot stall the per-endpoint registration loop.
            for DeferredOAuthInit {
                name,
                mut adapter,
                discovery,
            } in deferred_oauth_init
            {
                let reg = registry.clone();
                let settled = settled_inits.clone();
                let handle = tokio::spawn(async move {
                    if let Some((base, allow_insecure)) = discovery {
                        match crate::oauth::discovery::discover_authorization_server(
                            &base,
                            allow_insecure,
                        )
                        .await
                        {
                            Ok(disc) => {
                                info!(
                                    endpoint = %name,
                                    token_endpoint = %disc.token_endpoint,
                                    "RFC 8414 discovery resolved token endpoint at OAuth startup"
                                );
                                adapter
                                    .shared_inner()
                                    .set_token_endpoint_override(disc.token_endpoint)
                                    .await;
                            }
                            Err(e) => {
                                warn!(
                                    endpoint = %name,
                                    error = %e,
                                    "RFC 8414 discovery against oauth_server_url failed at OAuth startup; \
                                     falling back to convention-based token endpoint"
                                );
                            }
                        }
                    }
                    adapter.initialize().await.ok();
                    info!(endpoint = %name, "OAuth adapter initialized");
                    let mut entries = reg.entries().write().await;
                    if let Some(entry) = entries.get_mut(name.as_str()) {
                        entry.adapter = Box::new(adapter);
                        if entry.disabled {
                            let _ = entry.adapter.shutdown().await;
                        }
                    }
                    drop(entries);
                    reg.rewire_tools_changed_listener(&name).await;
                    reg.invalidate_catalog_cache().await;
                    settled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                });
                init_handles.push(handle);
            }
            let total_inits = init_handles.len();
            let js_execution_mode = Arc::new(AtomicBool::new(
                cfg.relay.local_js_execution.unwrap_or(false),
            ));
            let meta_tool_handler = Arc::new(MetaToolHandler::new(
                registry.clone(),
                Duration::from_secs(30),
            ));
            let setup_manager = Arc::new(OAuthSetupManager::new());
            // TOON output: CLI `--no-toon` forces off; otherwise honour
            // `relay.toon_output` from config.toml; default on when neither
            // is set.
            let toon_enabled = if no_toon {
                false
            } else {
                cfg.relay.toon_output.unwrap_or(true)
            };
            info!(toon_enabled, "TOON output conversion configured");
            let state = AppState {
                registry: (*registry).clone(),
                js_execution_mode: js_execution_mode.clone(),
                meta_tool_handler,
                oauth_flow_manager: Some(oauth_flow_manager.clone()),
                token_manager: Some(token_manager.clone()),
                oauth_adapter_inners: Some(oauth_adapter_inners.clone()),
                setup_manager: Some(setup_manager.clone()),
                started_at: std::time::Instant::now(),
                toon_enabled,
            };
            let mgmt_state = management::ManagementState {
                registry: registry.clone(),
                config: Arc::new(tokio::sync::RwLock::new(cfg.clone())),
                start_time: std::time::Instant::now(),
                config_path: Some(config_path.clone()),
                oauth_flow_manager: Some(oauth_flow_manager.clone()),
                relay_port: port,
                oauth_adapter_inners: Some(oauth_adapter_inners.clone()),
                token_manager: Some(token_manager.clone()),
                setup_manager: Some(setup_manager.clone()),
            };
            // Build the MCP (TCP) and management (UDS / Named Pipe) routers
            // separately. The management API carries credential-bearing routes
            // (`/api/*`) and is served exclusively over a local IPC transport
            // to eliminate the DNS-rebinding / CSRF attack surface — see
            // `management_listener` and the security audit (Cluster 1).
            let router = build_router(state);
            let mgmt_router = management::management_routes(mgmt_state);

            // Bind to loopback only; the relay is a local-only service.
            let addr: SocketAddr = ([127, 0, 0, 1], port).into();

            // Start the management listener on its IPC path. We do this
            // *before* binding the MCP TCP listener so that callers observing
            // the management socket see every endpoint in its
            // `Initializing` placeholder state immediately, even when the
            // background adapter inits take seconds to complete.
            let api_socket_path = management_listener::resolve_api_socket_path(&data_dir_path);
            let mgmt_handle = match management_listener::serve_management_api(
                mgmt_router,
                api_socket_path.clone(),
            )
            .await
            {
                Ok((path, h)) => {
                    info!(path = %path.display(), "Management API listener ready");
                    h
                }
                Err(e) => {
                    error!(error = %e, path = %api_socket_path.display(), "Failed to start management API listener");
                    std::process::exit(1);
                }
            };

            // Wait for adapter inits to settle, up to `startup_init_timeout_secs`
            // (default 60s). A configured value of `0` skips the wait
            // entirely and binds MCP TCP immediately. Dropping the
            // JoinHandles when the timeout fires does NOT abort the tasks —
            // late-arriving adapters keep running and publish their tools
            // via the registry's catalog invalidation tail.
            let timeout_secs = cfg
                .relay
                .startup_init_timeout_secs
                .unwrap_or(config::DEFAULT_STARTUP_INIT_TIMEOUT_SECS);
            let mut shutdown_during_wait = false;
            if total_inits > 0 && timeout_secs > 0 {
                let timeout = Duration::from_secs(timeout_secs);
                let wait_start = std::time::Instant::now();
                let all_settled = futures_util::future::join_all(init_handles);
                tokio::select! {
                    _ = all_settled => {
                        info!(
                            elapsed_ms = wait_start.elapsed().as_millis() as u64,
                            total = total_inits,
                            "All adapter initializations settled"
                        );
                    }
                    _ = tokio::time::sleep(timeout) => {
                        // Handles are dropped here; tasks keep running.
                    }
                    _ = server::shutdown_signal() => {
                        shutdown_during_wait = true;
                    }
                }
            } else if total_inits == 0 {
                info!("No adapter initializations to await");
            } else {
                info!("startup_init_timeout_secs=0; binding MCP TCP without waiting for adapter inits");
            }

            // Summarize Ready / Failed / still-Initializing endpoint counts
            // so operators can tell from the logs whether we bound TCP eagerly
            // because of the timeout.
            let (ready_n, failed_n, initializing_n) = {
                use adapter::HealthStatus;
                let entries = registry.entries().read().await;
                let mut ready = 0usize;
                let mut failed = 0usize;
                let mut initializing = 0usize;
                for (_, entry) in entries.iter() {
                    if entry.disabled {
                        continue;
                    }
                    match entry.adapter.health() {
                        HealthStatus::Healthy => ready += 1,
                        HealthStatus::Unhealthy(_) => failed += 1,
                        HealthStatus::Starting => initializing += 1,
                        HealthStatus::Stopped => {}
                    }
                }
                (ready, failed, initializing)
            };
            let settled_n = settled_inits.load(std::sync::atomic::Ordering::Relaxed);
            info!(
                ready = ready_n,
                failed = failed_n,
                initializing = initializing_n,
                settled_inits = settled_n,
                total_inits,
                timeout_secs,
                "Startup init phase complete"
            );

            if shutdown_during_wait {
                info!("Shutdown signal received during startup wait; tearing down");
                mgmt_handle.abort();
                let mut entries = registry.entries().write().await;
                for (name, entry) in entries.iter_mut() {
                    info!(endpoint = %name, "Shutting down adapter");
                    if let Err(e) = entry.adapter.shutdown().await {
                        warn!(endpoint = %name, error = %e, "Error shutting down adapter");
                    }
                }
                info!("All adapters shut down, exiting");
                return;
            }

            // Keep the management handle alive for the rest of the process.
            let _mgmt_handle = mgmt_handle;

            match start_server(router, addr).await {
                Ok((bound_addr, handle)) => {
                    info!(addr = %bound_addr, "MCP server running");

                    // Spawn config file watcher for hot-reload
                    let _watcher_handle = ConfigWatcher::start(
                        config_path.clone(),
                        registry.clone(),
                        cfg.relay.machine_name.clone(),
                        js_execution_mode.clone(),
                        token_manager.clone(),
                        oauth_flow_manager.clone(),
                        oauth_adapter_inners.clone(),
                    );

                    handle.await.ok();

                    // Shut down all adapters gracefully (kills STDIO child processes, etc.)
                    info!("Shutting down all adapters");
                    let mut entries = registry.entries().write().await;
                    for (name, entry) in entries.iter_mut() {
                        info!(endpoint = %name, "Shutting down adapter");
                        if let Err(e) = entry.adapter.shutdown().await {
                            warn!(endpoint = %name, error = %e, "Error shutting down adapter");
                        }
                    }
                    info!("All adapters shut down, exiting");
                }
                Err(e) => {
                    error!(error = %e, "Failed to start HTTP server");
                    std::process::exit(1);
                }
            }
        }
    }
}
