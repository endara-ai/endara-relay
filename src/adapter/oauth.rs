use super::http::{HttpAdapter, HttpConfig};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};

/// OAuth MCP adapter — wraps an HttpAdapter with Bearer token injection.
///
/// When no tokens are available, reports `Unhealthy("needs login")` and returns
/// empty tool lists / errors for tool calls. Once tokens arrive (via the watch
/// channel), it builds an inner HttpAdapter with the Bearer header and delegates.
pub struct OAuthAdapter {
    /// The endpoint name (used for logging).
    endpoint_name: String,
    /// URL of the upstream MCP server (e.g. http://localhost:5000/mcp).
    url: String,
    /// Current inner adapter (None until tokens are available).
    inner: Arc<RwLock<Option<HttpAdapter>>>,
    /// Receiver for token notifications. Sender is held by the callback handler.
    token_rx: watch::Receiver<Option<String>>,
    /// Current health status.
    health: Arc<RwLock<HealthStatus>>,
}

impl OAuthAdapter {
    /// Create a new OAuthAdapter.
    ///
    /// - `endpoint_name`: name of this endpoint in the registry.
    /// - `url`: the upstream MCP server URL.
    /// - `token_rx`: watch channel that receives the current access token (or None).
    pub fn new(
        endpoint_name: String,
        url: String,
        token_rx: watch::Receiver<Option<String>>,
    ) -> Self {
        Self {
            endpoint_name,
            url,
            inner: Arc::new(RwLock::new(None)),
            token_rx,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
        }
    }

    /// Build an inner HttpAdapter with a Bearer token in the default headers.
    fn build_inner(url: &str, access_token: &str) -> HttpAdapter {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                if let Ok(val) =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", access_token))
                {
                    headers.insert(reqwest::header::AUTHORIZATION, val);
                }
                headers
            })
            .build()
            .expect("failed to build HTTP client");
        HttpAdapter::new_with_client(HttpConfig::new(url), client)
    }
}

#[async_trait]
impl McpAdapter for OAuthAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        // Check if we already have a token
        let token = self.token_rx.borrow().clone();
        if let Some(ref access_token) = token {
            info!(endpoint = %self.endpoint_name, "OAuth adapter initializing with existing token");
            let mut adapter = Self::build_inner(&self.url, access_token);
            match adapter.initialize().await {
                Ok(()) => {
                    *self.inner.write().await = Some(adapter);
                    *self.health.write().await = HealthStatus::Healthy;
                    info!(endpoint = %self.endpoint_name, "OAuth adapter initialized");
                }
                Err(e) => {
                    let msg = format!("upstream init failed: {}", e);
                    *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                    warn!(endpoint = %self.endpoint_name, error = %e, "OAuth adapter upstream init failed");
                }
            }
        } else {
            // No tokens yet — that's OK, we stay in "needs login" state
            *self.health.write().await = HealthStatus::Unhealthy("needs login".to_string());
            info!(endpoint = %self.endpoint_name, "OAuth adapter awaiting login");
        }

        // Spawn a background task to watch for token changes
        let inner = self.inner.clone();
        let health = self.health.clone();
        let url = self.url.clone();
        let endpoint_name = self.endpoint_name.clone();
        let mut rx = self.token_rx.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let token = rx.borrow().clone();
                match token {
                    Some(access_token) => {
                        info!(endpoint = %endpoint_name, "Token received, reinitializing inner adapter");
                        let mut adapter = Self::build_inner(&url, &access_token);
                        match adapter.initialize().await {
                            Ok(()) => {
                                *inner.write().await = Some(adapter);
                                *health.write().await = HealthStatus::Healthy;
                                info!(endpoint = %endpoint_name, "Inner adapter reinitialized");
                            }
                            Err(e) => {
                                let msg = format!("upstream init failed: {}", e);
                                *health.write().await = HealthStatus::Unhealthy(msg);
                                warn!(endpoint = %endpoint_name, error = %e, "Failed to reinitialize");
                            }
                        }
                    }
                    None => {
                        *inner.write().await = None;
                        *health.write().await = HealthStatus::Unhealthy("needs login".to_string());
                    }
                }
            }
        });

        Ok(()) // initialize always succeeds for OAuth
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        let guard = self.inner.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.list_tools().await,
            None => Ok(vec![]),
        }
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
        let guard = self.inner.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.call_tool(name, arguments).await,
            None => Err(AdapterError::ConnectionFailed(
                "not authenticated — complete OAuth login first".to_string(),
            )),
        }
    }

    fn server_type(&self) -> Option<String> {
        self.inner
            .try_read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|a| a.server_type()))
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        let mut guard = self.inner.write().await;
        if let Some(ref mut adapter) = *guard {
            adapter.shutdown().await?;
        }
        *guard = None;
        *self.health.write().await = HealthStatus::Stopped;
        info!(endpoint = %self.endpoint_name, "OAuth adapter shut down");
        Ok(())
    }

    fn health(&self) -> HealthStatus {
        match self.health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        }
    }

    async fn activity_log(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        match guard.as_ref() {
            Some(adapter) => adapter.activity_log().await,
            None => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_watch() -> (
        watch::Sender<Option<String>>,
        watch::Receiver<Option<String>>,
    ) {
        watch::channel(None)
    }

    #[tokio::test]
    async fn health_no_tokens_is_unhealthy() {
        let (_tx, rx) = make_watch();
        let mut adapter = OAuthAdapter::new("test".into(), "http://localhost/mcp".into(), rx);
        adapter.initialize().await.unwrap();
        match adapter.health() {
            HealthStatus::Unhealthy(msg) => assert_eq!(msg, "needs login"),
            other => panic!("expected Unhealthy('needs login'), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn list_tools_no_tokens_returns_empty() {
        let (_tx, rx) = make_watch();
        let mut adapter = OAuthAdapter::new("test".into(), "http://localhost/mcp".into(), rx);
        adapter.initialize().await.unwrap();
        let tools = adapter.list_tools().await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn call_tool_no_tokens_returns_error() {
        let (_tx, rx) = make_watch();
        let mut adapter = OAuthAdapter::new("test".into(), "http://localhost/mcp".into(), rx);
        adapter.initialize().await.unwrap();
        let result = adapter.call_tool("any", serde_json::json!({})).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            AdapterError::ConnectionFailed(msg) => {
                assert!(msg.contains("not authenticated"));
            }
            other => panic!("expected ConnectionFailed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn health_with_token_but_unreachable_is_unhealthy() {
        // If we have a token but the upstream server is unreachable,
        // the adapter should report unhealthy after init
        let (tx, rx) = make_watch();
        tx.send(Some("fake-token".into())).unwrap();
        let mut adapter = OAuthAdapter::new("test".into(), "http://127.0.0.1:19999/mcp".into(), rx);
        adapter.initialize().await.unwrap();
        // Give a moment for init to complete
        tokio::time::sleep(Duration::from_millis(100)).await;
        match adapter.health() {
            HealthStatus::Unhealthy(msg) => {
                assert!(msg.contains("upstream init failed"));
            }
            other => panic!("expected Unhealthy, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn shutdown_sets_stopped() {
        let (_tx, rx) = make_watch();
        let mut adapter = OAuthAdapter::new("test".into(), "http://localhost/mcp".into(), rx);
        adapter.initialize().await.unwrap();
        adapter.shutdown().await.unwrap();
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }
}
