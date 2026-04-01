use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::{FailedAdapter, McpAdapter};
use crate::config::{self, ConfigDiff, EndpointConfig, Transport};
use crate::registry::AdapterRegistry;
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
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = watch_loop(config_path, registry, machine_name, js_execution_mode).await
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

        apply_diff_graceful(&diff, &registry, &warnings, &warned_names).await;

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
pub async fn apply_diff(diff: &ConfigDiff, registry: &AdapterRegistry) {
    // Remove endpoints
    for name in &diff.removed {
        info!(endpoint = %name, "Removing endpoint");
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(endpoint = %name, error = %e, "Error shutting down removed adapter");
            }
        }
    }

    // Changed endpoints: shutdown old, create new, preserve disabled state
    for (name, new_ep) in &diff.changed {
        info!(endpoint = %name, "Restarting changed endpoint");
        // Capture disabled state from old entry before removing
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
        let adapter = create_adapter(new_ep).await;
        registry
            .register(name.clone(), adapter, new_ep.transport.to_string(), new_ep.description.clone(), new_ep.resolved_tool_prefix())
            .await;
        // Re-apply disabled state
        let mut entries = registry.entries().write().await;
        if let Some(entry) = entries.get_mut(name.as_str()) {
            entry.disabled = was_disabled;
            entry.disabled_tools = old_disabled_tools;
            if was_disabled {
                let _ = entry.adapter.shutdown().await;
            }
        }
        info!(endpoint = %name, "Changed endpoint re-registered");
    }

    // Added endpoints
    for ep in &diff.added {
        info!(endpoint = %ep.name, transport = %ep.transport, "Adding new endpoint");
        let adapter = create_adapter(ep).await;
        registry
            .register(ep.name.clone(), adapter, ep.transport.to_string(), ep.description.clone(), ep.resolved_tool_prefix())
            .await;
        // Apply disabled state from config
        if ep.disabled || !ep.disabled_tools.is_empty() {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                entry.disabled = ep.disabled;
                entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
                if ep.disabled {
                    let _ = entry.adapter.shutdown().await;
                }
            }
        }
        info!(endpoint = %ep.name, "New endpoint registered");
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

    // Changed endpoints: shutdown old, create new, preserve disabled state
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

        let adapter: Box<dyn McpAdapter> = if warned_names.contains(name) {
            let msg = warning_messages.get(name).cloned().unwrap_or_default();
            warn!(endpoint = %name, "Registering as failed due to validation error: {}", msg);
            Box::new(FailedAdapter::new(msg))
        } else {
            create_adapter(new_ep).await
        };
        registry
            .register(name.clone(), adapter, new_ep.transport.to_string(), new_ep.description.clone(), new_ep.resolved_tool_prefix())
            .await;
        let mut entries = registry.entries().write().await;
        if let Some(entry) = entries.get_mut(name.as_str()) {
            entry.disabled = was_disabled;
            entry.disabled_tools = old_disabled_tools;
            if was_disabled {
                let _ = entry.adapter.shutdown().await;
            }
        }
        info!(endpoint = %name, "Changed endpoint re-registered");
    }

    // Added endpoints
    for ep in &diff.added {
        info!(endpoint = %ep.name, transport = %ep.transport, "Adding new endpoint");
        let adapter: Box<dyn McpAdapter> = if warned_names.contains(&ep.name) {
            let msg = warning_messages.get(&ep.name).cloned().unwrap_or_default();
            warn!(endpoint = %ep.name, "Registering as failed due to validation error: {}", msg);
            Box::new(FailedAdapter::new(msg))
        } else {
            create_adapter(ep).await
        };
        registry
            .register(ep.name.clone(), adapter, ep.transport.to_string(), ep.description.clone(), ep.resolved_tool_prefix())
            .await;
        if ep.disabled || !ep.disabled_tools.is_empty() {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                entry.disabled = ep.disabled;
                entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
                if ep.disabled {
                    let _ = entry.adapter.shutdown().await;
                }
            }
        }
        info!(endpoint = %ep.name, "New endpoint registered");
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
async fn create_adapter(ep: &EndpointConfig) -> Box<dyn McpAdapter> {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use async_trait::async_trait;
    use serde_json::json;

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

    fn make_tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{} tool", name)),
            input_schema: json!({"type": "object"}),
            annotations: None,
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

    #[tokio::test]
    async fn apply_diff_empty_is_noop() {
        let registry = Arc::new(AdapterRegistry::new());
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

        apply_diff(&empty_diff(), &registry).await;

        // Existing adapter should still be there, not shut down
        assert!(!shutdown.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(registry.merged_catalog().await.len(), 1);
    }

    #[tokio::test]
    async fn apply_diff_removes_endpoint() {
        let registry = Arc::new(AdapterRegistry::new());
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

        apply_diff(&diff, &registry).await;

        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
        assert!(registry.merged_catalog().await.is_empty());
    }

    #[tokio::test]
    async fn apply_diff_remove_nonexistent_is_ok() {
        let registry = Arc::new(AdapterRegistry::new());
        let diff = ConfigDiff {
            removed: vec!["ghost".to_string()],
            ..empty_diff()
        };

        // Should not panic
        apply_diff(&diff, &registry).await;
    }

    #[tokio::test]
    async fn apply_diff_changed_shuts_down_old() {
        let registry = Arc::new(AdapterRegistry::new());
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
        };
        let diff = ConfigDiff {
            changed: vec![("ep".to_string(), changed_ep)],
            ..empty_diff()
        };

        apply_diff(&diff, &registry).await;

        // Old adapter should have been shut down
        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn apply_diff_added_with_invalid_command_registers_as_failed() {
        let registry = Arc::new(AdapterRegistry::new());

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
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry).await;

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

        apply_diff(&diff, &registry).await;

        // "keep" should still be alive
        assert!(!shutdown_keep.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(registry.merged_catalog().await.len(), 1);
        // Single-server no-prefix mode: only one adapter remains, so no prefix
        assert_eq!(registry.merged_catalog().await[0].name, "t1");

        // "remove" should be shut down
        assert!(shutdown_remove.load(std::sync::atomic::Ordering::SeqCst));
    }
}
