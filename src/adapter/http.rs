use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::jsonrpc::{self, JsonRpcResponse};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// Configuration for the HTTP MCP adapter.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The URL of the HTTP MCP server endpoint (e.g., http://host:port/mcp).
    pub url: String,
    /// Request timeout in seconds (default: 30).
    pub timeout_secs: u64,
}

impl HttpConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_secs: 30,
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

/// HTTP MCP adapter — sends JSON-RPC requests as HTTP POST.
pub struct HttpAdapter {
    config: HttpConfig,
    client: Client,
    health: Arc<RwLock<HealthStatus>>,
    request_id: AtomicU64,
}

impl HttpAdapter {
    /// Create a new HttpAdapter with the given configuration.
    pub fn new(config: HttpConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build HTTP client");

        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Send a JSON-RPC request via HTTP POST and return the result.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let id = self.next_id();
        let request = jsonrpc::new_request(method, params, id);

        debug!(method = method, id = id, url = %self.config.url, "sending HTTP JSON-RPC request");

        let resp = self
            .client
            .post(&self.config.url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AdapterError::Timeout(self.config.timeout_secs)
                } else if e.is_connect() {
                    AdapterError::ConnectionFailed(format!("{}: {}", self.config.url, e))
                } else {
                    AdapterError::HttpError(e.to_string())
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AdapterError::HttpError(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let response: JsonRpcResponse = resp.json().await.map_err(|e| {
            AdapterError::ProtocolError(format!("invalid JSON-RPC response: {}", e))
        })?;

        if let Some(err) = response.error {
            return Err(AdapterError::JsonRpcError {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        response
            .result
            .ok_or_else(|| AdapterError::ProtocolError("response has no result".into()))
    }
}

#[async_trait]
impl McpAdapter for HttpAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Starting;

        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "endara-relay",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        match self.send_request("initialize", Some(params)).await {
            Ok(_result) => {
                *self.health.write().await = HealthStatus::Healthy;
                info!(url = %self.config.url, "HTTP MCP adapter initialized");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg);
                error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                Err(e)
            }
        }
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        let result = self.send_request("tools/list", None).await?;
        let tools_value = result
            .get("tools")
            .ok_or_else(|| AdapterError::ProtocolError("missing 'tools' field".into()))?;
        let tools: Vec<ToolInfo> = serde_json::from_value(tools_value.clone())?;
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        self.send_request("tools/call", Some(params)).await
    }

    fn health(&self) -> HealthStatus {
        match self.health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        }
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Stopped;
        info!(url = %self.config.url, "HTTP MCP adapter shut down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_config_defaults() {
        let config = HttpConfig::new("http://localhost:8080/mcp");
        assert_eq!(config.url, "http://localhost:8080/mcp");
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn test_http_config_with_timeout() {
        let config = HttpConfig::new("http://localhost:8080/mcp").with_timeout(60);
        assert_eq!(config.timeout_secs, 60);
    }

    #[test]
    fn test_http_adapter_initial_health() {
        let adapter = HttpAdapter::new(HttpConfig::new("http://localhost:8080/mcp"));
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[tokio::test]
    async fn test_http_adapter_connection_refused() {
        let mut adapter = HttpAdapter::new(HttpConfig::new("http://127.0.0.1:19999/mcp"));
        let result = adapter.initialize().await;
        assert!(result.is_err());
        match adapter.health() {
            HealthStatus::Unhealthy(_) => {}
            other => panic!("expected Unhealthy, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_http_adapter_shutdown() {
        let mut adapter = HttpAdapter::new(HttpConfig::new("http://localhost:8080/mcp"));
        adapter.shutdown().await.unwrap();
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }
}
