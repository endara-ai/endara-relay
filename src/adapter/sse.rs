use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::jsonrpc::{self, JsonRpcResponse};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// Configuration for the SSE MCP adapter.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// The SSE endpoint URL (e.g., http://host:port/sse).
    pub url: String,
    /// Request timeout in seconds for JSON-RPC POST calls (default: 30).
    pub timeout_secs: u64,
}

impl SseConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_secs: 30,
        }
    }

    #[allow(dead_code)] // Builder method kept for API completeness
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

/// Crash tracking for exponential backoff.
#[derive(Debug)]
#[allow(dead_code)] // Used by try_reconnect, kept for reconnection support
struct CrashTracker {
    timestamps: Vec<Instant>,
    consecutive_failures: u32,
}

impl CrashTracker {
    fn new() -> Self {
        Self {
            timestamps: Vec::new(),
            consecutive_failures: 0,
        }
    }

    #[allow(dead_code)] // Used by try_reconnect
    fn record_failure(&mut self) -> bool {
        let now = Instant::now();
        self.consecutive_failures += 1;
        self.timestamps.push(now);
        let cutoff = now - Duration::from_secs(60);
        self.timestamps.retain(|t| *t >= cutoff);
        self.timestamps.len() >= 3
    }

    #[allow(dead_code)] // Used by try_reconnect
    fn backoff_duration(&self) -> Duration {
        let secs = match self.consecutive_failures {
            0 | 1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 60,
        };
        Duration::from_secs(secs)
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
    }
}

/// SSE MCP adapter — connects to a remote SSE MCP server.
///
/// Protocol: GET /sse to receive event stream. The server sends an "endpoint"
/// event with the URL to POST JSON-RPC messages to. Responses come back
/// as "message" events on the SSE stream.
pub struct SseAdapter {
    config: SseConfig,
    client: Client,
    health: Arc<RwLock<HealthStatus>>,
    request_id: AtomicU64,
    /// The POST endpoint URL received from the SSE stream.
    post_endpoint: Arc<RwLock<Option<String>>>,
    /// Pending responses indexed by request ID.
    pending: Arc<Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>>,
    /// Handle for the background SSE listener task.
    sse_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    crash_tracker: Arc<Mutex<CrashTracker>>,
}

impl SseAdapter {
    pub fn new(config: SseConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build HTTP client");

        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
            post_endpoint: Arc::new(RwLock::new(None)),
            pending: Arc::new(Mutex::new(std::collections::HashMap::new())),
            sse_handle: Arc::new(Mutex::new(None)),
            crash_tracker: Arc::new(Mutex::new(CrashTracker::new())),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Resolve a relative endpoint URL against the SSE base URL.
    #[allow(dead_code)]
    fn resolve_endpoint(&self, endpoint: &str) -> String {
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            return endpoint.to_string();
        }
        // Relative URL — resolve against the base
        if let Ok(base) = url::Url::parse(&self.config.url) {
            if let Ok(resolved) = base.join(endpoint) {
                return resolved.to_string();
            }
        }
        // Fallback: just combine with base origin
        let base = &self.config.url;
        if let Some(idx) = base.rfind('/') {
            let origin = &base[..idx];
            format!("{}{}", origin, if endpoint.starts_with('/') { "" } else { "/" }).to_string()
                + endpoint
        } else {
            endpoint.to_string()
        }
    }

    /// Connect to the SSE endpoint and start listening for events.
    async fn connect(&self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Starting;

        // Build a long-lived GET request for SSE (no timeout for the stream itself)
        let sse_client = Client::builder()
            .build()
            .map_err(|e| AdapterError::ConnectionFailed(e.to_string()))?;

        let resp = sse_client
            .get(&self.config.url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    AdapterError::ConnectionFailed(format!("{}: {}", self.config.url, e))
                } else {
                    AdapterError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(AdapterError::ConnectionFailed(format!(
                "SSE endpoint returned HTTP {}",
                resp.status()
            )));
        }

        let (endpoint_tx, endpoint_rx) = tokio::sync::oneshot::channel::<String>();

        let pending = self.pending.clone();
        let post_endpoint = self.post_endpoint.clone();
        let health = self.health.clone();
        let base_url = self.config.url.clone();

        // Spawn SSE listener task
        let handle = tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut buffer = String::new();
            let mut event_type = String::new();
            let mut data_lines = Vec::<String>::new();
            let mut bytes_stream = resp.bytes_stream();

            use futures_util::StreamExt;
            while let Some(chunk_result) = bytes_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        error!(error = %e, "SSE stream error");
                        *health.write().await =
                            HealthStatus::Unhealthy(format!("SSE stream error: {}", e));
                        break;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete lines
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if line.is_empty() {
                        // Empty line = end of event
                        if !data_lines.is_empty() {
                            let data = data_lines.join("\n");
                            let etype = if event_type.is_empty() {
                                "message"
                            } else {
                                &event_type
                            };

                            match etype {
                                "endpoint" => {
                                    let endpoint_url = if data.starts_with("http://")
                                        || data.starts_with("https://")
                                    {
                                        data.clone()
                                    } else {
                                        // Resolve relative URL
                                        if let Ok(base) = url::Url::parse(&base_url) {
                                            base.join(&data)
                                                .map(|u| u.to_string())
                                                .unwrap_or(data.clone())
                                        } else {
                                            data.clone()
                                        }
                                    };
                                    debug!(endpoint = %endpoint_url, "received SSE endpoint");
                                    *post_endpoint.write().await = Some(endpoint_url.clone());
                                    if let Some(tx) = endpoint_tx.take() {
                                        let _ = tx.send(endpoint_url);
                                    }
                                }
                                "message" => {
                                    match serde_json::from_str::<JsonRpcResponse>(&data) {
                                        Ok(response) => {
                                            if let Some(id) = response.id {
                                                let mut map = pending.lock().await;
                                                if let Some(tx) = map.remove(&id) {
                                                    let _ = tx.send(response);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!(error = %e, data = %data, "failed to parse SSE message");
                                        }
                                    }
                                }
                                _ => {
                                    debug!(event_type = %etype, "ignoring SSE event");
                                }
                            }
                        }
                        event_type.clear();
                        data_lines.clear();
                    } else if let Some(rest) = line.strip_prefix("event:") {
                        event_type = rest.trim().to_string();
                    } else if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest.trim().to_string());
                    }
                    // Ignore other fields (id:, retry:, comments)
                }
            }

            *health.write().await = HealthStatus::Unhealthy("SSE stream closed".into());
        });

        *self.sse_handle.lock().await = Some(handle);

        // Wait for the endpoint event (with timeout)
        match tokio::time::timeout(Duration::from_secs(10), endpoint_rx).await {
            Ok(Ok(endpoint)) => {
                debug!(endpoint = %endpoint, "SSE endpoint received, connection established");
                Ok(())
            }
            Ok(Err(_)) => Err(AdapterError::ConnectionFailed(
                "SSE endpoint channel dropped".into(),
            )),
            Err(_) => Err(AdapterError::Timeout(10)),
        }
    }

    /// Send a JSON-RPC request via POST to the endpoint and wait for the response via SSE.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let endpoint = {
            let guard = self.post_endpoint.read().await;
            guard
                .clone()
                .ok_or(AdapterError::NotInitialized)?
        };

        let id = self.next_id();
        let request = jsonrpc::new_request(method, params, id);

        // Register a pending response channel
        let (tx, rx) = tokio::sync::oneshot::channel::<JsonRpcResponse>();
        self.pending.lock().await.insert(id, tx);

        debug!(method = method, id = id, endpoint = %endpoint, "sending SSE JSON-RPC request");

        // POST the request
        self.client
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    AdapterError::Timeout(self.config.timeout_secs)
                } else if e.is_connect() {
                    AdapterError::ConnectionFailed(format!("{}: {}", endpoint, e))
                } else {
                    AdapterError::HttpError(e.to_string())
                }
            })?;

        // Wait for response via SSE stream
        let response = tokio::time::timeout(Duration::from_secs(self.config.timeout_secs), rx)
            .await
            .map_err(|_| {
                // Clean up pending entry
                let pending = self.pending.clone();
                tokio::spawn(async move {
                    pending.lock().await.remove(&id);
                });
                AdapterError::Timeout(self.config.timeout_secs)
            })?
            .map_err(|_| AdapterError::ProtocolError("response channel dropped".into()))?;

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
impl McpAdapter for SseAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        if let Err(e) = self.connect().await {
            let msg = e.to_string();
            *self.health.write().await = HealthStatus::Unhealthy(msg);
            error!(url = %self.config.url, error = %e, "SSE connection failed");
            return Err(e);
        }

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
                self.crash_tracker.lock().await.reset();
                info!(url = %self.config.url, "SSE MCP adapter initialized");
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg);
                error!(url = %self.config.url, error = %e, "SSE MCP adapter initialization failed");
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

        // Abort the SSE listener task
        if let Some(handle) = self.sse_handle.lock().await.take() {
            handle.abort();
        }

        // Clear the endpoint
        *self.post_endpoint.write().await = None;

        // Drop all pending requests
        self.pending.lock().await.clear();

        info!(url = %self.config.url, "SSE MCP adapter shut down");
        Ok(())
    }
}

/// Attempt to reconnect after a failure with exponential backoff.
#[allow(dead_code)] // Kept for future reconnection support
pub async fn try_reconnect(adapter: &mut SseAdapter) -> Result<(), AdapterError> {
    let should_stop = {
        let mut tracker = adapter.crash_tracker.lock().await;
        let unhealthy = tracker.record_failure();
        if unhealthy {
            true
        } else {
            let backoff = tracker.backoff_duration();
            info!(backoff_secs = backoff.as_secs(), "backing off before SSE reconnect");
            drop(tracker);
            tokio::time::sleep(backoff).await;
            false
        }
    };

    if should_stop {
        let reason = "3+ failures in 60 seconds".to_string();
        *adapter.health.write().await = HealthStatus::Unhealthy(reason.clone());
        error!("SSE adapter marked unhealthy: {}", reason);
        return Err(AdapterError::ConnectionFailed(reason));
    }

    adapter.initialize().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_config_defaults() {
        let config = SseConfig::new("http://localhost:8080/sse");
        assert_eq!(config.url, "http://localhost:8080/sse");
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn test_sse_config_with_timeout() {
        let config = SseConfig::new("http://localhost:8080/sse").with_timeout(60);
        assert_eq!(config.timeout_secs, 60);
    }

    #[test]
    fn test_sse_adapter_initial_health() {
        let adapter = SseAdapter::new(SseConfig::new("http://localhost:8080/sse"));
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_resolve_endpoint_absolute() {
        let adapter = SseAdapter::new(SseConfig::new("http://localhost:8080/sse"));
        assert_eq!(
            adapter.resolve_endpoint("http://localhost:8080/message"),
            "http://localhost:8080/message"
        );
    }

    #[test]
    fn test_resolve_endpoint_relative() {
        let adapter = SseAdapter::new(SseConfig::new("http://localhost:8080/sse"));
        let resolved = adapter.resolve_endpoint("/message?sessionId=abc");
        assert!(resolved.starts_with("http://localhost:8080/message"));
    }

    #[tokio::test]
    async fn test_sse_adapter_connection_refused() {
        let mut adapter = SseAdapter::new(SseConfig::new("http://127.0.0.1:19998/sse"));
        let result = adapter.initialize().await;
        assert!(result.is_err());
        match adapter.health() {
            HealthStatus::Unhealthy(_) => {}
            other => panic!("expected Unhealthy, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_sse_adapter_shutdown() {
        let mut adapter = SseAdapter::new(SseConfig::new("http://localhost:8080/sse"));
        adapter.shutdown().await.unwrap();
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_crash_tracker_backoff() {
        let mut tracker = CrashTracker::new();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(1));
        tracker.record_failure();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(1));
        tracker.record_failure();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(2));
    }

    #[test]
    fn test_crash_tracker_marks_unhealthy() {
        let mut tracker = CrashTracker::new();
        assert!(!tracker.record_failure());
        assert!(!tracker.record_failure());
        assert!(tracker.record_failure()); // 3rd failure -> unhealthy
    }

    #[test]
    fn test_crash_tracker_reset() {
        let mut tracker = CrashTracker::new();
        tracker.record_failure();
        tracker.record_failure();
        tracker.reset();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(1));
    }
}
