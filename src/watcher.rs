use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::McpAdapter;
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

    // Load initial config as baseline
    let current_config = Arc::new(Mutex::new(config::load_config(&config_path)?));

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

        // Parse new config
        let new_config = match config::load_config(&config_path) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(error = %e, "Failed to parse updated config, keeping current config");
                continue;
            }
        };

        // Diff and apply
        let old_config = current_config.lock().await;
        let diff = config::diff_configs(&old_config, &new_config);
        drop(old_config);

        apply_diff(&diff, &registry).await;

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

    // Changed endpoints: shutdown old, create new
    for (name, new_ep) in &diff.changed {
        info!(endpoint = %name, "Restarting changed endpoint");
        if let Some(mut entry) = registry.remove(name).await {
            if let Err(e) = entry.adapter.shutdown().await {
                warn!(endpoint = %name, error = %e, "Error shutting down old adapter");
            }
        }
        if let Some(adapter) = create_adapter(new_ep).await {
            registry
                .register(name.clone(), adapter, new_ep.transport.to_string())
                .await;
            info!(endpoint = %name, "Changed endpoint re-registered");
        }
    }

    // Added endpoints
    for ep in &diff.added {
        info!(endpoint = %ep.name, transport = %ep.transport, "Adding new endpoint");
        if let Some(adapter) = create_adapter(ep).await {
            registry
                .register(ep.name.clone(), adapter, ep.transport.to_string())
                .await;
            info!(endpoint = %ep.name, "New endpoint registered");
        }
    }

    // Log unchanged
    for name in &diff.unchanged {
        info!(endpoint = %name, "Endpoint unchanged, keeping running");
    }
}

/// Create an adapter from an endpoint configuration.
async fn create_adapter(ep: &EndpointConfig) -> Option<Box<dyn McpAdapter>> {
    match ep.transport {
        Transport::Stdio => {
            let stdio_config = StdioConfig {
                command: ep.command.clone().unwrap_or_default(),
                args: ep.args.clone().unwrap_or_default(),
                env: ep.env.clone().unwrap_or_default(),
            };
            let mut adapter = StdioAdapter::new(stdio_config);
            match adapter.initialize().await {
                Ok(()) => Some(Box::new(adapter)),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize stdio adapter");
                    None
                }
            }
        }
        Transport::Sse => {
            let url = ep.url.clone().unwrap_or_default();
            let mut adapter = SseAdapter::new(SseConfig::new(url));
            match adapter.initialize().await {
                Ok(()) => Some(Box::new(adapter)),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize SSE adapter");
                    None
                }
            }
        }
        Transport::Http => {
            let url = ep.url.clone().unwrap_or_default();
            let mut adapter = HttpAdapter::new(HttpConfig::new(url));
            match adapter.initialize().await {
                Ok(()) => Some(Box::new(adapter)),
                Err(e) => {
                    warn!(endpoint = %ep.name, error = %e, "Failed to initialize HTTP adapter");
                    None
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
        let registry = Arc::new(AdapterRegistry::new("m".into()));
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "existing".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
            )
            .await;

        apply_diff(&empty_diff(), &registry).await;

        // Existing adapter should still be there, not shut down
        assert!(!shutdown.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(registry.merged_catalog().await.len(), 1);
    }

    #[tokio::test]
    async fn apply_diff_removes_endpoint() {
        let registry = Arc::new(AdapterRegistry::new("m".into()));
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "to_remove".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
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
        let registry = Arc::new(AdapterRegistry::new("m".into()));
        let diff = ConfigDiff {
            removed: vec!["ghost".to_string()],
            ..empty_diff()
        };

        // Should not panic
        apply_diff(&diff, &registry).await;
    }

    #[tokio::test]
    async fn apply_diff_changed_shuts_down_old() {
        let registry = Arc::new(AdapterRegistry::new("m".into()));
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")], shutdown.clone())),
                "stdio".into(),
            )
            .await;

        // Change the endpoint config — create_adapter will fail to spawn a real process,
        // but the old adapter should be shut down and removed
        let changed_ep = EndpointConfig {
            name: "ep".to_string(),
            transport: Transport::Stdio,
            command: Some("/nonexistent/binary/that/wont/start".to_string()),
            args: None,
            url: None,
            env: None,
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
    async fn apply_diff_added_with_invalid_command_does_not_register() {
        let registry = Arc::new(AdapterRegistry::new("m".into()));

        let new_ep = EndpointConfig {
            name: "bad_ep".to_string(),
            transport: Transport::Stdio,
            command: Some("/nonexistent/binary/that/wont/start".to_string()),
            args: None,
            url: None,
            env: None,
        };
        let diff = ConfigDiff {
            added: vec![new_ep],
            ..empty_diff()
        };

        apply_diff(&diff, &registry).await;

        // The failed adapter should not appear in the registry
        assert!(registry.merged_catalog().await.is_empty());
    }

    #[tokio::test]
    async fn apply_diff_preserves_unchanged_endpoints() {
        let registry = Arc::new(AdapterRegistry::new("m".into()));
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
        assert_eq!(registry.merged_catalog().await[0].name, "m__keep__t1");

        // "remove" should be shut down
        assert!(shutdown_remove.load(std::sync::atomic::Ordering::SeqCst));
    }
}
