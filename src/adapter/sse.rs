use super::server_name::{sanitize_server_name, ServerNameError};
use super::stdio::RingBuffer;
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::jsonrpc::{self, JsonRpcResponse};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

/// Configuration for the SSE MCP adapter.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// The SSE endpoint URL (e.g., http://host:port/sse).
    pub url: String,
    /// Request timeout in seconds for JSON-RPC POST calls (default: 30).
    pub timeout_secs: u64,
    /// Custom HTTP headers to include in requests.
    pub headers: HashMap<String, String>,
}

impl SseConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_secs: 30,
            headers: HashMap::new(),
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
struct CrashTracker {
    timestamps: Vec<Instant>,
    consecutive_failures: u32,
    failure_window: Duration,
    max_failures_in_window: usize,
    base_backoff: Duration,
}

impl CrashTracker {
    fn new() -> Self {
        Self {
            timestamps: Vec::new(),
            consecutive_failures: 0,
            failure_window: Duration::from_secs(60),
            max_failures_in_window: 3,
            base_backoff: Duration::from_secs(1),
        }
    }

    /// Record a failure and return true when the window cap is reached.
    fn record_failure(&mut self) -> bool {
        let now = Instant::now();
        self.consecutive_failures += 1;
        self.timestamps.push(now);
        let cutoff = now.checked_sub(self.failure_window).unwrap_or(now);
        self.timestamps.retain(|t| *t >= cutoff);
        self.timestamps.len() >= self.max_failures_in_window
    }

    fn backoff_duration(&self) -> Duration {
        let multiplier = match self.consecutive_failures {
            0 | 1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 60,
        };
        self.base_backoff.saturating_mul(multiplier)
    }

    fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.timestamps.clear();
    }

    /// Build a tracker with custom timing knobs (for fast unit tests).
    #[cfg(test)]
    fn new_test(base_backoff: Duration, max_failures: usize, window: Duration) -> Self {
        Self {
            timestamps: Vec::new(),
            consecutive_failures: 0,
            failure_window: window,
            max_failures_in_window: max_failures,
            base_backoff,
        }
    }
}

/// SSE MCP adapter — connects to a remote SSE MCP server.
///
/// Protocol: GET /sse to receive event stream. The server sends an "endpoint"
/// event with the URL to POST JSON-RPC messages to. Responses come back
/// as "message" events on the SSE stream.
///
/// All fields are `Arc`-wrapped so the adapter can be cheaply cloned into the
/// background reconnect supervisor task.
#[derive(Clone)]
pub struct SseAdapter {
    config: SseConfig,
    client: Client,
    health: Arc<RwLock<HealthStatus>>,
    request_id: Arc<AtomicU64>,
    /// The POST endpoint URL received from the SSE stream.
    post_endpoint: Arc<RwLock<Option<String>>>,
    /// Pending responses indexed by request ID.
    pending:
        Arc<Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<JsonRpcResponse>>>>,
    /// Handle for the background SSE listener task.
    sse_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    crash_tracker: Arc<Mutex<CrashTracker>>,
    /// Sanitized server name from the MCP initialize response.
    server_type: Arc<RwLock<Option<String>>>,
    /// Ring buffer recording tool call activity.
    activity_log: Arc<RwLock<RingBuffer>>,
    /// Handle for the background reconnect supervisor task.
    reconnect_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Notified by the listener when the SSE stream dies and a reconnect is needed.
    reconnect_notify: Arc<Notify>,
    /// Notified by `shutdown()` so the supervisor can exit cleanly.
    shutdown_notify: Arc<Notify>,
}

impl SseAdapter {
    /// Build a `reqwest::header::HeaderMap` from config headers, skipping Content-Type.
    fn build_header_map(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (key, value) in headers {
            if key.eq_ignore_ascii_case("content-type") {
                warn!(header = %key, "Ignoring custom Content-Type header; JSON-RPC requires application/json");
                continue;
            }
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                header_map.insert(name, val);
            } else {
                warn!(header = %key, "Invalid header name or value, skipping");
            }
        }
        header_map
    }

    pub fn new(config: SseConfig) -> Self {
        let default_headers = Self::build_header_map(&config.headers);
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .default_headers(default_headers)
            .build()
            .expect("failed to build HTTP client");

        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: Arc::new(AtomicU64::new(1)),
            post_endpoint: Arc::new(RwLock::new(None)),
            pending: Arc::new(Mutex::new(std::collections::HashMap::new())),
            sse_handle: Arc::new(Mutex::new(None)),
            crash_tracker: Arc::new(Mutex::new(CrashTracker::new())),
            server_type: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
            reconnect_handle: Arc::new(Mutex::new(None)),
            reconnect_notify: Arc::new(Notify::new()),
            shutdown_notify: Arc::new(Notify::new()),
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
            format!(
                "{}{}",
                origin,
                if endpoint.starts_with('/') { "" } else { "/" }
            )
            .to_string()
                + endpoint
        } else {
            endpoint.to_string()
        }
    }

    /// Connect to the SSE endpoint and start listening for events.
    async fn connect(&self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Starting;

        // Build a long-lived GET request for SSE (no timeout for the stream itself)
        let sse_headers = Self::build_header_map(&self.config.headers);
        let sse_client = Client::builder()
            .default_headers(sse_headers)
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
                    AdapterError::HttpError {
                        status: 0,
                        body: e.to_string(),
                    }
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
        let reconnect_notify = self.reconnect_notify.clone();
        let base_url = self.config.url.clone();

        // Spawn SSE listener task
        let handle = tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut buffer = String::new();
            let mut event_type = String::new();
            let mut data_lines = Vec::<String>::new();
            let mut bytes_stream = resp.bytes_stream();
            let mut close_reason: Option<String> = None;

            use futures_util::StreamExt;
            while let Some(chunk_result) = bytes_stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(error = %e, "SSE stream error");
                        close_reason = Some(format!("SSE stream error: {}", e));
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
                                "message" => match serde_json::from_str::<JsonRpcResponse>(&data) {
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
                                },
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

            // Stream ended (Err or end-of-stream). Mark unhealthy, clear the
            // POST endpoint so new requests fail fast, drain pending requests
            // with errors so callers don't hang, and signal the reconnect
            // supervisor to attempt recovery.
            let reason =
                close_reason.unwrap_or_else(|| "SSE stream closed, reconnecting".to_string());
            *post_endpoint.write().await = None;
            {
                let mut map = pending.lock().await;
                if !map.is_empty() {
                    debug!(
                        count = map.len(),
                        "draining pending SSE requests after stream death"
                    );
                }
                map.clear();
            }
            *health.write().await = HealthStatus::Unhealthy(reason);
            reconnect_notify.notify_one();
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
            guard.clone().ok_or(AdapterError::NotInitialized)?
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
                    AdapterError::HttpError {
                        status: 0,
                        body: e.to_string(),
                    }
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

    /// Connect to the SSE endpoint and perform the MCP initialize handshake.
    ///
    /// Used by `initialize()` for the first connection and by the reconnect
    /// supervisor task to re-establish a healthy connection. On success, sets
    /// `health = Healthy` and resets the crash tracker. On failure, sets
    /// `health = Unhealthy(...)` with the underlying error.
    async fn connect_and_handshake(&self) -> Result<(), AdapterError> {
        if let Err(e) = self.connect().await {
            let msg = e.to_string();
            *self.health.write().await = HealthStatus::Unhealthy(msg);
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

        let result = match self.send_request("initialize", Some(params)).await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg);
                return Err(e);
            }
        };

        // Extract serverInfo.name — REQUIRED per MCP spec enforcement
        let raw_name = match result
            .get("serverInfo")
            .and_then(|si| si.get("name"))
            .and_then(|n| n.as_str())
        {
            Some(n) => n.to_string(),
            None => {
                let msg = ServerNameError::Missing.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                return Err(AdapterError::ProtocolError(msg));
            }
        };

        let sanitized = match sanitize_server_name(&raw_name) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                return Err(AdapterError::ProtocolError(msg));
            }
        };

        debug!(url = %self.config.url, raw_name = %raw_name, sanitized = %sanitized, "MCP server reported serverInfo.name");
        *self.server_type.write().await = Some(sanitized);
        *self.health.write().await = HealthStatus::Healthy;
        self.crash_tracker.lock().await.reset();
        Ok(())
    }

    /// Spawn the background reconnect supervisor task if it isn't running.
    async fn ensure_supervisor_running(&self) {
        let mut guard = self.reconnect_handle.lock().await;
        if guard.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let me = self.clone();
        let handle = tokio::spawn(async move {
            me.run_supervisor().await;
        });
        *guard = Some(handle);
    }

    /// Reconnect supervisor loop — wait for stream-death notifications and
    /// attempt to re-establish the connection with exponential backoff. Exits
    /// when the failure cap is reached or shutdown is signaled.
    async fn run_supervisor(&self) {
        loop {
            tokio::select! {
                _ = self.shutdown_notify.notified() => return,
                _ = self.reconnect_notify.notified() => {}
            }

            loop {
                let (cap_reached, backoff) = {
                    let mut tracker = self.crash_tracker.lock().await;
                    let reached = tracker.record_failure();
                    (reached, tracker.backoff_duration())
                };

                if cap_reached {
                    let reason = "SSE reconnect cap reached; manual reconnect required".to_string();
                    warn!(url = %self.config.url, "{}", reason);
                    *self.health.write().await = HealthStatus::Unhealthy(reason);
                    return;
                }

                info!(
                    url = %self.config.url,
                    backoff_ms = backoff.as_millis() as u64,
                    "SSE reconnect: backing off before next attempt"
                );

                tokio::select! {
                    _ = self.shutdown_notify.notified() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }

                info!(url = %self.config.url, "SSE reconnect: attempting");
                match self.connect_and_handshake().await {
                    Ok(()) => {
                        info!(url = %self.config.url, "SSE reconnect succeeded");
                        break;
                    }
                    Err(e) => {
                        warn!(url = %self.config.url, error = %e, "SSE reconnect attempt failed");
                    }
                }
            }
        }
    }
}

#[async_trait]
impl McpAdapter for SseAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        if let Err(e) = self.connect_and_handshake().await {
            error!(url = %self.config.url, error = %e, "SSE MCP adapter initialization failed");
            return Err(e);
        }

        self.ensure_supervisor_running().await;
        info!(url = %self.config.url, "SSE MCP adapter initialized");
        Ok(())
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
        let start = Instant::now();
        let result = self.send_request("tools/call", Some(params)).await;
        let duration_ms = start.elapsed().as_millis();
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let log_line = match &result {
            Ok(_) => format!(
                "{}  INFO call_tool tool={} status=ok duration={}ms",
                now, name, duration_ms
            ),
            Err(e) => format!(
                "{}  WARN call_tool tool={} status=error duration={}ms error={}",
                now, name, duration_ms, e
            ),
        };
        self.activity_log.write().await.push(log_line);
        result
    }

    fn health(&self) -> HealthStatus {
        match self.health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        }
    }

    fn server_type(&self) -> Option<String> {
        self.server_type.try_read().ok().and_then(|g| g.clone())
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Stopped;

        // Tell the supervisor to wake up and exit (in case it's sleeping in
        // backoff or waiting on a reconnect notification).
        self.shutdown_notify.notify_waiters();

        // Abort the SSE listener task
        if let Some(handle) = self.sse_handle.lock().await.take() {
            handle.abort();
        }

        // Abort the reconnect supervisor task
        if let Some(handle) = self.reconnect_handle.lock().await.take() {
            handle.abort();
        }

        // Clear the endpoint
        *self.post_endpoint.write().await = None;

        // Drop all pending requests
        self.pending.lock().await.clear();

        info!(url = %self.config.url, "SSE MCP adapter shut down");
        Ok(())
    }

    async fn activity_log(&self) -> Vec<String> {
        self.activity_log
            .read()
            .await
            .lines()
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
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

    // -----------------------------------------------------------------
    // Reconnect supervisor tests — exercise the full listener/supervisor
    // loop against a tiny in-process axum SSE server.
    // -----------------------------------------------------------------

    mod reconnect {
        use super::*;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::sse::{Event, KeepAlive, Sse};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::convert::Infallible;
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        use tokio::net::TcpListener;
        use tokio::sync::{broadcast, mpsc};

        /// Behavior knobs for the test SSE server.
        #[derive(Default)]
        struct FakeServerState {
            /// Number of GET /sse connections accepted so far.
            connections: AtomicUsize,
            /// When > 0, GET /sse returns 503 once `connections > healthy_until`.
            healthy_until: AtomicUsize,
            /// When true, /sse closes the stream right after sending endpoint.
            close_immediately: AtomicBool,
            /// When true, /message closes the active SSE stream on `tools/call`
            /// without broadcasting a response.
            close_on_tools_call: AtomicBool,
            /// When true, /message does NOT broadcast `tools/call` responses
            /// (used to test pending-request error propagation).
            silent_on_tools_call: AtomicBool,
        }

        /// Handle for a running test server; aborts on drop.
        struct FakeServer {
            url: String,
            state: Arc<FakeServerState>,
            close_tx: broadcast::Sender<()>,
            handle: tokio::task::JoinHandle<()>,
        }

        impl Drop for FakeServer {
            fn drop(&mut self) {
                self.handle.abort();
            }
        }

        impl FakeServer {
            /// Force-close all active SSE streams.
            fn close_all_streams(&self) {
                let _ = self.close_tx.send(());
            }
        }

        #[derive(Clone)]
        struct AppState {
            fake: Arc<FakeServerState>,
            // Broadcast channel: /message writes responses, SSE handlers forward.
            response_tx: broadcast::Sender<Value>,
            // Broadcast: when sent, all SSE handlers close their streams.
            close_tx: broadcast::Sender<()>,
        }

        async fn handle_sse(State(app): State<AppState>) -> axum::response::Response {
            let n = app.fake.connections.fetch_add(1, Ordering::SeqCst) + 1;
            let healthy_until = app.fake.healthy_until.load(Ordering::SeqCst);
            if healthy_until > 0 && n > healthy_until {
                return (StatusCode::SERVICE_UNAVAILABLE, "go away").into_response();
            }

            let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);
            let _ = tx
                .send(Ok(Event::default().event("endpoint").data("/message")))
                .await;

            if app.fake.close_immediately.load(Ordering::SeqCst) {
                drop(tx);
                return Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
                    .keep_alive(KeepAlive::default())
                    .into_response();
            }

            let mut response_rx = app.response_tx.subscribe();
            let mut close_rx = app.close_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = close_rx.recv() => break,
                        msg = response_rx.recv() => match msg {
                            Ok(value) => {
                                let data = serde_json::to_string(&value).unwrap_or_default();
                                if tx
                                    .send(Ok(Event::default().event("message").data(data)))
                                    .await
                                    .is_err()
                                { break; }
                            }
                            Err(_) => break,
                        },
                    }
                }
            });

            Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
                .keep_alive(KeepAlive::default())
                .into_response()
        }

        async fn handle_message(
            State(app): State<AppState>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let method = body["method"].as_str().unwrap_or("").to_string();
            let id = body["id"].as_u64().unwrap_or(0);
            let response = match method.as_str() {
                "initialize" => json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-sse", "version": "0.0.0"}
                    },
                    "id": id,
                }),
                _ => json!({
                    "jsonrpc": "2.0",
                    "result": {"content": [{"type": "text", "text": "ok"}]},
                    "id": id,
                }),
            };

            let is_tools_call = method == "tools/call";
            let silent = is_tools_call && app.fake.silent_on_tools_call.load(Ordering::SeqCst);
            if !silent {
                let _ = app.response_tx.send(response.clone());
            }
            if is_tools_call && app.fake.close_on_tools_call.load(Ordering::SeqCst) {
                let _ = app.close_tx.send(());
            }
            Json(response)
        }

        async fn start_test_server() -> FakeServer {
            let state = Arc::new(FakeServerState::default());
            let (response_tx, _) = broadcast::channel::<Value>(64);
            let (close_tx, _) = broadcast::channel::<()>(8);
            let app_state = AppState {
                fake: state.clone(),
                response_tx,
                close_tx: close_tx.clone(),
            };
            let app = Router::new()
                .route("/sse", get(handle_sse))
                .route("/message", post(handle_message))
                .with_state(app_state);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("http://{}/sse", addr);
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            FakeServer {
                url,
                state,
                close_tx,
                handle,
            }
        }

        /// Build an adapter wired to the given URL with a fast crash tracker
        /// (50ms base backoff, 3-failure cap in a 5s window) and a 1s
        /// per-request timeout — keeps tests bounded to a few hundred ms.
        async fn build_adapter(url: &str, max_failures: usize) -> SseAdapter {
            let adapter = SseAdapter::new(SseConfig::new(url).with_timeout(1));
            *adapter.crash_tracker.lock().await = CrashTracker::new_test(
                Duration::from_millis(50),
                max_failures,
                Duration::from_secs(5),
            );
            adapter
        }

        /// Poll `health()` until the predicate succeeds or the budget elapses.
        async fn wait_for_health<F: Fn(&HealthStatus) -> bool>(
            adapter: &SseAdapter,
            pred: F,
            budget: Duration,
        ) -> HealthStatus {
            let deadline = std::time::Instant::now() + budget;
            loop {
                let h = adapter.health();
                if pred(&h) {
                    return h;
                }
                if std::time::Instant::now() >= deadline {
                    return h;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        #[tokio::test]
        async fn sse_auto_reconnects_after_stream_drops() {
            let server = start_test_server().await;
            let mut adapter = build_adapter(&server.url, 5).await;

            adapter.initialize().await.expect("initial connect");
            assert_eq!(adapter.health(), HealthStatus::Healthy);

            // Force-close the active SSE stream — the listener should die,
            // mark unhealthy, and the supervisor should reconnect.
            server.close_all_streams();

            let h = wait_for_health(
                &adapter,
                |h| matches!(h, HealthStatus::Unhealthy(_)),
                Duration::from_secs(2),
            )
            .await;
            assert!(
                matches!(h, HealthStatus::Unhealthy(_)),
                "expected Unhealthy after stream close, got {:?}",
                h
            );

            let h = wait_for_health(
                &adapter,
                |h| *h == HealthStatus::Healthy,
                Duration::from_secs(3),
            )
            .await;
            assert_eq!(
                h,
                HealthStatus::Healthy,
                "expected Healthy after auto-reconnect"
            );

            // At least 2 GET /sse connections (initial + reconnect).
            assert!(server.state.connections.load(Ordering::SeqCst) >= 2);
            adapter.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn sse_stops_reconnecting_after_cap() {
            let server = start_test_server().await;
            // First connection is healthy; subsequent connections return 503.
            server.state.healthy_until.store(1, Ordering::SeqCst);
            let mut adapter = build_adapter(&server.url, 3).await;

            adapter.initialize().await.expect("initial connect");
            assert_eq!(adapter.health(), HealthStatus::Healthy);

            server.close_all_streams();

            // Wait long enough for the cap to be hit. Backoffs are 50/50/100ms
            // so 3 failed attempts complete well under 1.5s.
            let h = wait_for_health(
                &adapter,
                |h| {
                    matches!(
                        h,
                        HealthStatus::Unhealthy(reason) if reason.contains("cap reached")
                    )
                },
                Duration::from_secs(3),
            )
            .await;
            assert!(
                matches!(
                    h,
                    HealthStatus::Unhealthy(ref reason) if reason.contains("cap reached")
                ),
                "expected cap-reached Unhealthy, got {:?}",
                h
            );

            // Health remains Unhealthy — confirm by sampling again after a delay.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));

            // Supervisor task must have terminated after hitting the cap.
            {
                let guard = adapter.reconnect_handle.lock().await;
                let handle = guard
                    .as_ref()
                    .expect("supervisor handle should still be tracked post-cap");
                assert!(
                    handle.is_finished(),
                    "supervisor should exit after failure cap"
                );
            }

            // Subsequent stream-death notifications must NOT restart the
            // supervisor or trigger further GET /sse attempts.
            let connections_before = server.state.connections.load(Ordering::SeqCst);
            adapter.reconnect_notify.notify_waiters();
            adapter.reconnect_notify.notify_one();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                server.state.connections.load(Ordering::SeqCst),
                connections_before,
                "supervisor must not restart after cap"
            );

            adapter.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn sse_pending_requests_error_on_stream_death() {
            let server = start_test_server().await;
            // /message will not broadcast tools/call responses, and will
            // close the SSE stream as soon as a tools/call POST arrives.
            server
                .state
                .silent_on_tools_call
                .store(true, Ordering::SeqCst);
            server
                .state
                .close_on_tools_call
                .store(true, Ordering::SeqCst);

            let mut adapter = build_adapter(&server.url, 5).await;
            adapter.initialize().await.expect("initial connect");

            let start = std::time::Instant::now();
            let res = adapter.call_tool("echo", json!({"message": "hi"})).await;
            let elapsed = start.elapsed();

            assert!(res.is_err(), "expected call_tool to error, got {:?}", res);
            // Should be well under the 1s per-call timeout — drain happens fast.
            assert!(
                elapsed < Duration::from_millis(900),
                "call_tool took too long: {:?}",
                elapsed
            );
            adapter.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn sse_shutdown_aborts_reconnect_task() {
            let server = start_test_server().await;
            let mut adapter = build_adapter(&server.url, 10).await;
            adapter.initialize().await.expect("initial connect");

            // Capture supervisor handle before shutdown.
            let supervisor = adapter
                .reconnect_handle
                .lock()
                .await
                .as_ref()
                .map(|h| h.id());
            assert!(supervisor.is_some(), "supervisor should be running");

            // Close the stream so the supervisor enters its retry loop, then
            // immediately call shutdown() — it must abort cleanly.
            server.close_all_streams();
            tokio::time::sleep(Duration::from_millis(20)).await;

            let start = std::time::Instant::now();
            adapter.shutdown().await.unwrap();
            let elapsed = start.elapsed();

            assert!(
                elapsed < Duration::from_millis(500),
                "shutdown took too long: {:?}",
                elapsed
            );
            assert!(adapter.reconnect_handle.lock().await.is_none());
            assert!(adapter.sse_handle.lock().await.is_none());
            assert_eq!(adapter.health(), HealthStatus::Stopped);
        }

        #[tokio::test]
        async fn crash_tracker_reset_on_successful_reconnect() {
            let server = start_test_server().await;
            let mut adapter = build_adapter(&server.url, 5).await;
            adapter.initialize().await.expect("initial connect");
            assert_eq!(adapter.health(), HealthStatus::Healthy);

            // Drive a single failure cycle: close the stream, wait for the
            // supervisor to flag Unhealthy, then wait for a successful
            // auto-reconnect back to Healthy.
            server.close_all_streams();
            let h = wait_for_health(
                &adapter,
                |h| matches!(h, HealthStatus::Unhealthy(_)),
                Duration::from_secs(2),
            )
            .await;
            assert!(matches!(h, HealthStatus::Unhealthy(_)));
            let h = wait_for_health(
                &adapter,
                |h| *h == HealthStatus::Healthy,
                Duration::from_secs(3),
            )
            .await;
            assert_eq!(h, HealthStatus::Healthy);

            // After the successful reconnect, the crash tracker must be
            // reset so the next failure cycle starts with a full attempt
            // budget rather than 1 attempt left.
            {
                let tracker = adapter.crash_tracker.lock().await;
                assert_eq!(
                    tracker.consecutive_failures, 0,
                    "tracker must reset consecutive_failures on success"
                );
                assert!(
                    tracker.timestamps.is_empty(),
                    "tracker must clear failure timestamps on success"
                );
            }

            adapter.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn supervisor_handles_multiple_reconnect_cycles() {
            let server = start_test_server().await;
            let mut adapter = build_adapter(&server.url, 5).await;
            adapter.initialize().await.expect("initial connect");

            for cycle in 0..3 {
                server.close_all_streams();
                let h = wait_for_health(
                    &adapter,
                    |h| matches!(h, HealthStatus::Unhealthy(_)),
                    Duration::from_secs(2),
                )
                .await;
                assert!(
                    matches!(h, HealthStatus::Unhealthy(_)),
                    "cycle {}: expected Unhealthy after stream close, got {:?}",
                    cycle,
                    h
                );
                let h = wait_for_health(
                    &adapter,
                    |h| *h == HealthStatus::Healthy,
                    Duration::from_secs(3),
                )
                .await;
                assert_eq!(
                    h,
                    HealthStatus::Healthy,
                    "cycle {}: expected Healthy after auto-reconnect",
                    cycle
                );
            }

            // No pending requests should have leaked across cycles.
            assert!(
                adapter.pending.lock().await.is_empty(),
                "pending requests leaked across reconnect cycles"
            );

            // The same supervisor task should still be servicing reconnects;
            // a fresh listener handle should be tracked from the latest cycle.
            {
                let sup = adapter.reconnect_handle.lock().await;
                assert!(
                    sup.as_ref().is_some_and(|h| !h.is_finished()),
                    "supervisor should still be running after multiple cycles"
                );
            }
            assert!(
                adapter.sse_handle.lock().await.is_some(),
                "listener handle should be present after final reconnect"
            );

            assert_eq!(adapter.health(), HealthStatus::Healthy);
            // Initial connect plus 3 reconnects = at least 4 SSE GETs.
            assert!(
                server.state.connections.load(Ordering::SeqCst) >= 4,
                "expected >=4 SSE connections across 3 reconnect cycles"
            );

            adapter.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn shutdown_cancels_in_flight_backoff() {
            let server = start_test_server().await;
            // Only the first SSE GET succeeds; further attempts return 503,
            // so the supervisor records a failure and parks in the backoff
            // sleep before its next reconnect attempt.
            server.state.healthy_until.store(1, Ordering::SeqCst);

            // Long base backoff so the supervisor is solidly inside
            // tokio::time::sleep(backoff) when shutdown() is called.
            let mut adapter = SseAdapter::new(SseConfig::new(&server.url).with_timeout(1));
            *adapter.crash_tracker.lock().await =
                CrashTracker::new_test(Duration::from_secs(5), 10, Duration::from_secs(60));
            adapter.initialize().await.expect("initial connect");

            server.close_all_streams();

            // Wait until the listener has died (Unhealthy) and the supervisor
            // has had a chance to record the first failure and enter sleep.
            let h = wait_for_health(
                &adapter,
                |h| matches!(h, HealthStatus::Unhealthy(_)),
                Duration::from_secs(2),
            )
            .await;
            assert!(matches!(h, HealthStatus::Unhealthy(_)));
            tokio::time::sleep(Duration::from_millis(50)).await;

            // shutdown() must cancel the multi-second backoff sleep
            // immediately — well below the configured 5s base backoff.
            let start = std::time::Instant::now();
            adapter.shutdown().await.unwrap();
            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_millis(200),
                "shutdown did not cancel in-flight backoff: {:?}",
                elapsed
            );
            assert_eq!(adapter.health(), HealthStatus::Stopped);
        }
    }
}
