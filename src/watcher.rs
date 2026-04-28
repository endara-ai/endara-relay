use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::oauth::{OAuthAdapter, OAuthAdapterConfig};
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::{FailedAdapter, McpAdapter, StartingAdapter};
use crate::config::{self, ConfigDiff, EndpointConfig, Transport};
use crate::oauth::OAuthFlowManager;
use crate::registry::AdapterRegistry;
use crate::token_manager::TokenManager;
use crate::OAuthAdapterInners;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tracing::{error, info, warn};

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
    pub fn start(
        config_path: PathBuf,
        registry: Arc<AdapterRegistry>,
        machine_name: String,
        js_execution_mode: Arc<AtomicBool>,
        token_manager: Arc<TokenManager>,
        _oauth_flow_manager: Arc<OAuthFlowManager>,
        oauth_adapter_inners: OAuthAdapterInners,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = watch_loop(
                config_path,
                registry,
                machine_name,
                js_execution_mode,
                token_manager,
                oauth_adapter_inners,
            )
            .await
            {
                error!(error = %e, "Config watcher terminated with error");
            }
        })
    }
}

async fn watch_loop(
    config_path: PathBuf,
    registry: Arc<AdapterRegistry>,
    _machine_name: String,
    js_execution_mode: Arc<AtomicBool>,
    token_manager: Arc<TokenManager>,
    oauth_adapter_inners: OAuthAdapterInners,
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

    // Load initial config as baseline (use graceful — fatal errors still propagate)
    let (initial_config, initial_warnings) = config::load_config_graceful(&config_path)?;
    if !initial_warnings.is_empty() {
        for w in &initial_warnings {
            warn!("{}", w);
        }
    }
    let current_config = Arc::new(Mutex::new(initial_config));

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

        // Parse new config gracefully
        let (new_config, warnings) = match config::load_config_graceful(&config_path) {
            Ok(result) => result,
            Err(e) => {
                warn!(error = %e, "Failed to parse updated config, keeping current config");
                continue;
            }
        };

        for w in &warnings {
            warn!("{}", w);
        }

        let warned_names = config::warned_endpoint_names(&warnings);

        // Diff and apply
        let old_config = current_config.lock().await;
        let diff = config::diff_configs(&old_config, &new_config);
        drop(old_config);

        apply_diff_graceful(
            &diff,
            &registry,
            &warnings,
            &warned_names,
            &token_manager,
            &oauth_adapter_inners,
        )
        .await;

        // Update JS execution mode flag if it changed
        let new_js_mode = new_config.relay.local_js_execution.unwrap_or(false);
        let old_js_mode = js_execution_mode.load(Ordering::Relaxed);
        if new_js_mode != old_js_mode {
            js_execution_mode.store(new_js_mode, Ordering::Relaxed);
            info!(js_execution_mode = new_js_mode, "JS execution mode updated");
        }

        // Update baseline
        *current_config.lock().await = new_config;
    }

    Ok(())
}

/// Apply a config diff to the adapter registry.
///
/// This is public so it can also be called from a manual reload endpoint.
#[allow(dead_code)]
pub async fn apply_diff(
    diff: &ConfigDiff,
    registry: &AdapterRegistry,
    token_manager: &Arc<TokenManager>,
    oauth_adapter_inners: &OAuthAdapterInners,
) {
    // Remove endpoints
    for name in &diff.removed {
        info!(endpoint = %name, "Removing endpoint");
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(endpoint = %name, error = %e, "Error shutting down removed adapter");
            }
        }
    }

    // Changed endpoints: shutdown old, register as Starting, init in background
    for (name, new_ep) in &diff.changed {
        info!(endpoint = %name, "Restarting changed endpoint");
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
                warn!(endpoint = %name, error = %e, "Error shutting down old adapter");
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
        tokio::spawn(async move {
            let adapter = create_adapter(&ep_clone, &tm, &oai).await;
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
            info!(endpoint = %name_clone, "Changed endpoint initialized");
        });
    }

    // Added endpoints
    for ep in &diff.added {
        info!(endpoint = %ep.name, transport = %ep.transport, "Adding new endpoint");

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
        tokio::spawn(async move {
            let adapter = create_adapter(&ep_clone, &tm, &oai).await;
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
            info!(endpoint = %ep_clone.name, "New endpoint initialized");
        });
    }

    // Log unchanged
    for name in &diff.unchanged {
        info!(endpoint = %name, "Endpoint unchanged, keeping running");
    }
}

/// Like [`apply_diff`] but also handles per-endpoint validation warnings.
///
/// Endpoints whose names appear in `warned_names` are registered as `FailedAdapter`
/// with the warning message instead of attempting initialization.
pub async fn apply_diff_graceful(
    diff: &ConfigDiff,
    registry: &AdapterRegistry,
    warnings: &[config::EndpointValidationWarning],
    warned_names: &std::collections::HashSet<String>,
    token_manager: &Arc<TokenManager>,
    oauth_adapter_inners: &OAuthAdapterInners,
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
        info!(endpoint = %name, "Removing endpoint");
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(endpoint = %name, error = %e, "Error shutting down removed adapter");
            }
        }
    }

    // Changed endpoints: shutdown old, register as Starting, init in background
    for (name, new_ep) in &diff.changed {
        info!(endpoint = %name, "Restarting changed endpoint");
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
                warn!(endpoint = %name, error = %e, "Error shutting down old adapter");
            }
        }

        // Warned endpoints get FailedAdapter immediately (no background init)
        if warned_names.contains(name) {
            let msg = warning_messages.get(name).cloned().unwrap_or_default();
            warn!(endpoint = %name, "Registering as failed due to validation error: {}", msg);
            registry
                .register(
                    name.clone(),
                    Box::new(FailedAdapter::new(msg)),
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
            info!(endpoint = %name, "Changed endpoint re-registered (failed)");
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
        tokio::spawn(async move {
            let adapter = create_adapter(&ep_clone, &tm, &oai).await;
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
            info!(endpoint = %name_clone, "Changed endpoint initialized");
        });
    }

    // Added endpoints
    for ep in &diff.added {
        info!(endpoint = %ep.name, transport = %ep.transport, "Adding new endpoint");

        // Warned endpoints get FailedAdapter immediately
        if warned_names.contains(&ep.name) {
            let msg = warning_messages.get(&ep.name).cloned().unwrap_or_default();
            warn!(endpoint = %ep.name, "Registering as failed due to validation error: {}", msg);
            registry
                .register(
                    ep.name.clone(),
                    Box::new(FailedAdapter::new(msg)),
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
            info!(endpoint = %ep.name, "New endpoint registered (failed)");
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
        tokio::spawn(async move {
            let adapter = create_adapter(&ep_clone, &tm, &oai).await;
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
            info!(endpoint = %ep_clone.name, "New endpoint initialized");
        });
    }

    // Log unchanged
    for name in &diff.unchanged {
        info!(endpoint = %name, "Endpoint unchanged, keeping running");
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
) -> Box<dyn McpAdapter> {
    match ep.transport {
        Transport::Stdio => {
            let stdio_config = StdioConfig {
                command: ep.command.clone().unwrap_or_default(),
                args: ep.args.clone().unwrap_or_default(),
                env: ep.env.clone().unwrap_or_default(),
            };
            let mut adapter = StdioAdapter::new(stdio_config);
            match adapter.initialize().await {
                Ok(()) => Box::new(adapter),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize stdio adapter, registering as failed");
                    Box::new(FailedAdapter::new(e.to_string()))
                }
            }
        }
        Transport::Sse => {
            let url = ep.url.clone().unwrap_or_default();
            let mut sse_config = SseConfig::new(url);
            sse_config.headers = ep.headers.clone().unwrap_or_default();
            let mut adapter = SseAdapter::new(sse_config);
            match adapter.initialize().await {
                Ok(()) => Box::new(adapter),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize SSE adapter, registering as failed");
                    Box::new(FailedAdapter::new(e.to_string()))
                }
            }
        }
        Transport::Http => {
            let url = ep.url.clone().unwrap_or_default();
            let mut http_config = HttpConfig::new(url);
            http_config.headers = ep.headers.clone().unwrap_or_default();
            let mut adapter = HttpAdapter::new(http_config);
            match adapter.initialize().await {
                Ok(()) => Box::new(adapter),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize HTTP adapter, registering as failed");
                    Box::new(FailedAdapter::new(e.to_string()))
                }
            }
        }
        Transport::Oauth => {
            let oauth_config = OAuthAdapterConfig {
                endpoint_name: ep.name.clone(),
                url: ep.url.clone().unwrap_or_default(),
                token_endpoint_url: ep.token_endpoint.clone().unwrap_or_else(|| {
                    format!(
                        "{}/token",
                        ep.oauth_server_url.as_deref().unwrap_or_default()
                    )
                }),
                client_id: ep.client_id.clone().unwrap_or_default(),
                client_secret: ep.client_secret.clone(),
                heartbeat_interval_secs: 30,
                probe_timeout_secs: 10,
                probe_failure_threshold: 3,
            };

            let mut adapter = OAuthAdapter::new(oauth_config, token_manager.clone());
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

        apply_diff(&empty_diff(), &registry, &tm, &inners).await;

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

        apply_diff(&diff, &registry, &tm, &inners).await;

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
        apply_diff(&diff, &registry, &tm, &inners).await;
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
        };
        let diff = ConfigDiff {
            changed: vec![("ep".to_string(), changed_ep)],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners).await;

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
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners).await;

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

        apply_diff(&diff, &registry, &tm, &inners).await;

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
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry, &tm, &inners).await;

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

        apply_diff_graceful(&diff, &registry, &[], &Default::default(), &tm, &inners).await;

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

        apply_diff_graceful(&diff, &registry, &warnings, &warned, &tm, &inners).await;

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

        apply_diff_graceful(&diff, &registry, &warnings, &warned, &tm, &inners).await;

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
}
