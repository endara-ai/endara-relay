use super::server_name::{sanitize_server_name, ServerNameError};
use super::server_type_resolution::{effective_server_type, strip_mcp_server_suffix};
use super::stdio::{iso8601_now, RingBuffer};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo, DISCOVER_PROBE_TIMEOUT};
use crate::events::{
    annotations_from_value, current_request_context, ToolCallEvent, ToolCallEventBus,
};
use crate::jsonrpc::{self, JsonRpcResponse};
use crate::protocol::{self, detect_upstream_dialect, ProtocolVersion};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, Notify, RwLock};
use tokio::time::Instant;
use tracing::{debug, error, info, warn, Instrument};

/// Configuration for the SSE MCP adapter.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// The SSE endpoint URL (e.g., http://host:port/sse).
    pub url: String,
    /// Request timeout in seconds for JSON-RPC POST calls (default: 30).
    pub timeout_secs: u64,
    /// Custom HTTP headers to include in requests.
    pub headers: HashMap<String, String>,
    /// Optional override for the advertised `server_type` name. See
    /// [`crate::adapter::server_type_resolution::effective_server_type`].
    pub server_type_override: Option<String>,
    /// Endpoint name (used as the `endpoint` field on the adapter's
    /// per-endpoint `tracing` span). Defaults to empty for direct test
    /// construction; production paths set this from `EndpointConfig::name`.
    pub endpoint_name: String,
}

impl SseConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout_secs: 30,
            headers: HashMap::new(),
            server_type_override: None,
            endpoint_name: String::new(),
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
    /// Upstream-derived server name (sanitized + suffix-stripped), captured
    /// before any `server_type_override` resolution. See
    /// [`McpAdapter::upstream_server_name`].
    upstream_server_name: Arc<RwLock<Option<String>>>,
    /// Ring buffer recording tool call activity.
    activity_log: Arc<RwLock<RingBuffer>>,
    /// Handle for the background reconnect supervisor task.
    reconnect_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Notified by the listener when the SSE stream dies and a reconnect is needed.
    reconnect_notify: Arc<Notify>,
    /// Notified by `shutdown()` so the supervisor can exit cleanly.
    shutdown_notify: Arc<Notify>,
    /// Broadcast emitter for `notifications/tools/list_changed` events. Each
    /// tick is an opaque cache-invalidation signal consumed by the registry.
    tools_changed_tx: broadcast::Sender<()>,
    /// Per-endpoint tracing span. Every adapter method instruments its async
    /// body with this span so events carry `endpoint`/`transport` (and
    /// `server_type` once the MCP handshake completes).
    span: tracing::Span,
    /// Shared typed event bus for the desktop overlay's SSE stream. See
    /// the matching field on [`super::stdio::StdioAdapter`].
    event_bus: Arc<OnceLock<ToolCallEventBus>>,
    /// Per-tool annotation cache populated from `list_tools()` responses so
    /// `call_tool` can attach hint metadata to the overlay's `started`
    /// event without a second round-trip.
    tool_annotations_cache: Arc<RwLock<HashMap<String, Option<Value>>>>,
    /// Negotiated protocol dialect of the upstream server. Defaults to the
    /// legacy `2024-11-05` version this adapter advertises in `initialize`;
    /// real negotiation populates it via [`Self::set_upstream_dialect`] (T7).
    /// Consumed by the 2026 outbound code paths (T9).
    upstream_dialect: Arc<RwLock<ProtocolVersion>>,
    /// Upstream `ttlMs` freshness hint (SEP-2549) captured from the most recent
    /// successful `tools/list` result. `Some(ms)` only for 2026 upstreams that
    /// sent a top-level `ttlMs`; `None` otherwise. Read by the registry cache to
    /// honor the upstream's freshness window. See [`Self::list_tools_ttl_ms`].
    list_ttl_ms: Arc<RwLock<Option<u64>>>,
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

        let (tools_changed_tx, _) = broadcast::channel(16);

        let span = tracing::info_span!(
            "endpoint",
            endpoint = %config.endpoint_name,
            transport = "sse",
            server_type = tracing::field::Empty,
        );
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
            upstream_server_name: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
            reconnect_handle: Arc::new(Mutex::new(None)),
            reconnect_notify: Arc::new(Notify::new()),
            shutdown_notify: Arc::new(Notify::new()),
            tools_changed_tx,
            span,
            event_bus: Arc::new(OnceLock::new()),
            tool_annotations_cache: Arc::new(RwLock::new(HashMap::new())),
            upstream_dialect: Arc::new(RwLock::new(ProtocolVersion::V2024_11_05)),
            list_ttl_ms: Arc::new(RwLock::new(None)),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Record the upstream server's negotiated [`ProtocolVersion`]. Populated
    /// during the connect/handshake path (T7); consumed by the 2026 outbound
    /// code paths (T9).
    pub(crate) async fn set_upstream_dialect(&self, dialect: ProtocolVersion) {
        *self.upstream_dialect.write().await = dialect;
    }

    /// Read the upstream server's negotiated [`ProtocolVersion`]. Defaults to
    /// the legacy version this adapter advertises until T7/T9 populates it.
    #[allow(dead_code)]
    pub(crate) async fn upstream_dialect(&self) -> ProtocolVersion {
        *self.upstream_dialect.read().await
    }

    /// The relay's own client identity, injected under
    /// `params._meta["io.modelcontextprotocol/clientInfo"]` on every outbound
    /// request to a 2026 upstream. The 2026 transport is stateless — there is
    /// no `initialize` handshake — and the SSE transport carries no per-request
    /// HTTP headers for identity, so it travels per-request inside `_meta`.
    fn relay_client_info() -> Value {
        json!({
            "name": "endara-relay",
            "version": env!("CARGO_PKG_VERSION"),
        })
    }

    /// Attach the relay's `clientInfo` under `params._meta` for 2026 upstreams,
    /// creating an empty params object when the request carried none. Non-object
    /// params are left untouched (MCP params are always objects or absent).
    fn inject_client_info(params: Option<Value>) -> Option<Value> {
        let mut params = params.unwrap_or_else(|| json!({}));
        if params.is_object() {
            // Normalize `_meta` to a JSON object before the nested assignment:
            // serde_json's `IndexMut` panics on `value[key] = ...` when the
            // existing value is a non-object/non-null (e.g. an inbound 2026
            // request that already carries `params._meta` as a String/Array/
            // number/bool). Replace only a missing/null or non-object `_meta`;
            // a pre-existing object `_meta` (W3C Trace Context siblings) is
            // preserved so the clientInfo key is added alongside them.
            if !params["_meta"].is_object() {
                params["_meta"] = json!({});
            }
            params["_meta"][protocol::META_CLIENT_INFO_KEY] = Self::relay_client_info();
        }
        Some(params)
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
        let tools_changed_tx = self.tools_changed_tx.clone();

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
                                "message" => match serde_json::from_str::<Value>(&data) {
                                    Ok(value) => {
                                        let id_opt = value.get("id").and_then(|v| v.as_u64());
                                        let method_opt =
                                            value.get("method").and_then(|v| v.as_str());
                                        if id_opt.is_none() {
                                            // No id → JSON-RPC notification (or malformed
                                            // response). Surface tools-changed ticks; log
                                            // others.
                                            match method_opt {
                                                Some("notifications/tools/list_changed") => {
                                                    debug!(
                                                        "received tools/list_changed notification"
                                                    );
                                                    let _ = tools_changed_tx.send(());
                                                }
                                                Some(method) => {
                                                    debug!(method = %method, "ignoring SSE notification");
                                                }
                                                None => {
                                                    warn!(data = %data, "SSE message has no id and no method");
                                                }
                                            }
                                        } else {
                                            match serde_json::from_value::<JsonRpcResponse>(value) {
                                                Ok(response) => {
                                                    if let Some(id) = response.id {
                                                        let mut map = pending.lock().await;
                                                        if let Some(tx) = map.remove(&id) {
                                                            let _ = tx.send(response);
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    warn!(error = %e, data = %data, "failed to parse SSE response");
                                                }
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

    /// Send a JSON-RPC notification via POST to the endpoint. Notifications
    /// have no `id` and do not produce a response — we only inspect the HTTP
    /// status. Used for `notifications/initialized` after the handshake.
    async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), AdapterError> {
        // 2026 upstreams: attach `_meta` clientInfo on notifications too, so the
        // upstream sees the relay's identity per-message. Legacy: unchanged.
        let params = if self.upstream_dialect.read().await.is_2026() {
            Self::inject_client_info(params)
        } else {
            params
        };

        let endpoint = {
            let guard = self.post_endpoint.read().await;
            guard.clone().ok_or(AdapterError::NotInitialized)?
        };

        let mut request = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(p) = params {
            request["params"] = p;
        }

        debug!(method = method, endpoint = %endpoint, "sending SSE JSON-RPC notification");

        let resp = self
            .client
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

        let status = resp.status();
        if status.is_success() {
            debug!(method = method, status = %status, "notification accepted");
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(AdapterError::HttpError {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Send a JSON-RPC request via POST to the endpoint and wait for the response via SSE.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        // 2026 upstreams: every request carries the relay's `clientInfo` under
        // `params._meta` (there is no handshake). SSE carries no per-request
        // HTTP headers, so version/identity travel entirely in `_meta`.
        // Legacy: unchanged.
        let params = if self.upstream_dialect.read().await.is_2026() {
            Self::inject_client_info(params)
        } else {
            params
        };

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

    /// Stateless `server/discover` probe used to detect a 2026 upstream before
    /// the legacy `initialize` handshake. The request carries the relay's
    /// `_meta` clientInfo (SSE has no per-request HTTP headers for identity, so
    /// version/identity travel entirely in `params._meta`). Returns the
    /// JSON-RPC `result` object on success, or `None` on any failure (JSON-RPC
    /// error, transport failure, missing result) so the caller falls back to the
    /// legacy handshake. Legacy servers reject `server/discover` and the relay
    /// falls back transparently.
    async fn try_discover_probe(&self) -> Option<Value> {
        // Build params with `_meta` clientInfo explicitly: the upstream dialect
        // is still the legacy default here, so `send_request` would not inject
        // it for us, and a 2026 server expects identity on every request.
        let params = Self::inject_client_info(None);
        // Bound the probe with a short dedicated timeout (below the full
        // `timeout_secs` transport timeout) so an unresponsive/legacy upstream
        // that silently drops the unknown request falls back to the legacy
        // handshake fast. A timeout maps to `None`, the same clean legacy
        // fallback as any other failure.
        match tokio::time::timeout(
            DISCOVER_PROBE_TIMEOUT,
            self.send_request("server/discover", params),
        )
        .await
        {
            Ok(res) => res.ok(),
            Err(_) => None,
        }
    }

    /// Extract, validate, and record the upstream `serverInfo.name` from an
    /// `initialize` or `server/discover` result. Returns `Err` when the name is
    /// missing or fails sanitization. Shared by the legacy handshake and the
    /// 2026 stateless path so both name the endpoint identically. Does not touch
    /// `health`; the caller maps any error onto `HealthStatus::Unhealthy`.
    async fn apply_server_identity(&self, result: &Value) -> Result<(), AdapterError> {
        // Extract serverInfo.name — REQUIRED per MCP spec enforcement
        let raw_name = result
            .get("serverInfo")
            .and_then(|si| si.get("name"))
            .and_then(|n| n.as_str())
            .ok_or_else(|| AdapterError::ProtocolError(ServerNameError::Missing.to_string()))?;

        let sanitized = sanitize_server_name(raw_name)
            .map_err(|e| AdapterError::ProtocolError(e.to_string()))?;

        if let Some(ref ov) = self.config.server_type_override {
            if sanitize_server_name(ov).is_err() {
                warn!(
                    override = %ov,
                    "server_type_override failed sanitization; falling back to upstream-derived name"
                );
            }
        }
        let effective = effective_server_type(
            self.config.server_type_override.clone(),
            Some(sanitized.clone()),
        );
        let upstream_stripped = strip_mcp_server_suffix(sanitized.clone());

        debug!(url = %self.config.url, raw_name = %raw_name, sanitized = %sanitized, effective = ?effective, "MCP server reported serverInfo.name");
        if let Some(ref name) = effective {
            self.span
                .record("server_type", tracing::field::display(name));
        }
        *self.server_type.write().await = effective;
        *self.upstream_server_name.write().await = Some(upstream_stripped);
        Ok(())
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

        // Discover-first dialect detection (T9): probe `server/discover` before
        // the legacy handshake. A 2026 upstream answers with a `protocolVersion`
        // of `2026-07-28`, in which case the relay skips the `initialize`/
        // `notifications/initialized` handshake entirely — the 2026 transport is
        // stateless, carrying version + identity in `params._meta` on every
        // request instead. Any other outcome (legacy result, JSON-RPC error,
        // transport failure) falls through to the unchanged legacy handshake.
        let discover_result = self.try_discover_probe().await;
        if detect_upstream_dialect(discover_result.as_ref(), None).is_2026() {
            let result = discover_result.as_ref().expect(
                "detect_upstream_dialect reports 2026 only when a discover result is present",
            );
            self.set_upstream_dialect(ProtocolVersion::V2026_07_28)
                .await;
            if let Err(e) = self.apply_server_identity(result).await {
                let msg = e.to_string();
                *self.health.write().await = HealthStatus::Unhealthy(msg);
                return Err(e);
            }
            // 2026 is stateless: no notifications/initialized handshake.
            *self.health.write().await = HealthStatus::Healthy;
            self.crash_tracker.lock().await.reset();
            let _ = self.tools_changed_tx.send(());
            info!(url = %self.config.url, "MCP initialize skipped (2026 stateless path)");
            return Ok(());
        }

        let params = json!({
            "protocolVersion": ProtocolVersion::V2024_11_05.as_str(),
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

        // Validate + record the upstream serverInfo.name (REQUIRED per MCP spec
        // enforcement). Shared with the 2026 stateless path above; map any error
        // onto the adapter's health before returning.
        if let Err(e) = self.apply_server_identity(&result).await {
            let msg = e.to_string();
            *self.health.write().await = HealthStatus::Unhealthy(msg);
            return Err(e);
        }

        // Detect and record the upstream's negotiated protocol dialect. The
        // discover probe ran above (legacy result or none) and the initialize
        // result carries the negotiated legacy version; neither is 2026 here.
        self.set_upstream_dialect(detect_upstream_dialect(
            discover_result.as_ref(),
            Some(&result),
        ))
        .await;

        // Per the MCP spec the client MUST send a notifications/initialized
        // notification after a successful initialize exchange. Failure is
        // non-fatal — log and continue, matching the STDIO/HTTP adapters.
        if let Err(e) = self
            .send_notification("notifications/initialized", None)
            .await
        {
            warn!(url = %self.config.url, error = %e, "failed to send notifications/initialized");
        }

        *self.health.write().await = HealthStatus::Healthy;
        self.crash_tracker.lock().await.reset();
        // Emit a tick after every successful (re)connect + handshake so any
        // stale post-disconnect tools cache gets invalidated. The registry's
        // listener loop is idempotent; a no-op invalidation on an empty cache
        // is harmless. `SendError` (no subscribers) is harmless — drop it.
        let _ = self.tools_changed_tx.send(());
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
        async {
            if let Err(e) = self.connect_and_handshake().await {
                error!(url = %self.config.url, error = %e, "SSE MCP adapter initialization failed");
                return Err(e);
            }

            self.ensure_supervisor_running().await;
            info!(url = %self.config.url, "SSE MCP adapter initialized");
            Ok(())
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        async {
            let result = self.send_request("tools/list", None).await?;
            let tools_value = result
                .get("tools")
                .ok_or_else(|| AdapterError::ProtocolError("missing 'tools' field".into()))?;
            let tools: Vec<ToolInfo> = serde_json::from_value(tools_value.clone())?;
            // Capture the upstream `ttlMs` freshness hint (SEP-2549) only for
            // 2026 upstreams; legacy upstreams never carry it and keep the
            // existing event-driven cache behavior. Read by the registry cache.
            let ttl = if self.upstream_dialect.read().await.is_2026() {
                protocol::ttl_ms_from_result(&result)
            } else {
                None
            };
            *self.list_ttl_ms.write().await = ttl;
            // Refresh the per-tool annotations cache for overlay events.
            let mut cache = self.tool_annotations_cache.write().await;
            cache.clear();
            for tool in &tools {
                cache.insert(tool.name.clone(), tool.annotations.clone());
            }
            drop(cache);
            Ok(tools)
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_tools_ttl_ms(&self) -> Option<u64> {
        *self.list_ttl_ms.read().await
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
        self.call_tool_with_request_params(name, arguments, serde_json::Map::new())
            .await
    }

    async fn call_tool_with_request_params(
        &self,
        name: &str,
        arguments: Value,
        request_params: serde_json::Map<String, Value>,
    ) -> Result<Value, AdapterError> {
        // Capture caller span context BEFORE `.instrument(self.span)` re-enters
        // the adapter's own `endpoint` span — endpoint is constructed at
        // adapter init time with no parent linkage to per-request spans, so
        // reading the context from inside the instrumented body would lose
        // the `request{id}` / `mcp_request{profile}` scope.
        // See `events::SpanFieldCaptureLayer`.
        let span_ctx = current_request_context();
        async {
            let request_id = uuid::Uuid::new_v4().to_string();
            if let Some(bus) = self.event_bus.get() {
                let annotations = self
                    .tool_annotations_cache
                    .read()
                    .await
                    .get(name)
                    .and_then(|v| v.as_ref().and_then(annotations_from_value));
                bus.send(ToolCallEvent::Started {
                    request_id: request_id.clone(),
                    request_uid: span_ctx.request_uid.clone(),
                    ts: iso8601_now(),
                    endpoint: self.config.endpoint_name.clone(),
                    transport: "sse".into(),
                    server_type: self.server_type.read().await.clone(),
                    server_name: self.upstream_server_name.read().await.clone(),
                    profile: span_ctx.profile.clone(),
                    tool: name.to_string(),
                    annotations,
                    client: span_ctx.client.clone(),
                });
            }
            let mut params = json!({
                "name": name,
                "arguments": arguments,
            });
            crate::adapter::merge_request_params(&mut params, request_params);
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
            let client_name = span_ctx
                .client
                .as_ref()
                .and_then(|c| c.client_label())
                .unwrap_or_default();
            let client_version = span_ctx
                .client
                .as_ref()
                .and_then(|c| c.version.clone())
                .unwrap_or_default();
            match &result {
                Ok(_) => tracing::info!(
                    tool = %name,
                    status = "ok",
                    duration_ms = duration_ms,
                    client_name = ?client_name,
                    client_version = ?client_version,
                    "Tool call completed"
                ),
                Err(e) => tracing::warn!(
                    tool = %name,
                    status = "error",
                    duration_ms = duration_ms,
                    error = %e,
                    client_name = ?client_name,
                    client_version = ?client_version,
                    "Tool call failed"
                ),
            }
            if let Some(bus) = self.event_bus.get() {
                let duration_ms_u64 = duration_ms as u64;
                let ts = iso8601_now();
                match &result {
                    Ok(_) => bus.send(ToolCallEvent::Completed {
                        request_id,
                        ts,
                        duration_ms: duration_ms_u64,
                        status: "ok".into(),
                    }),
                    Err(e) => bus.send(ToolCallEvent::Failed {
                        request_id,
                        ts,
                        duration_ms: duration_ms_u64,
                        status: "error".into(),
                        error_message: e.to_string(),
                    }),
                }
            }
            result
        }
        .instrument(self.span.clone())
        .await
    }

    fn set_event_bus(&self, bus: ToolCallEventBus) {
        let _ = self.event_bus.set(bus);
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

    fn upstream_server_name(&self) -> Option<String> {
        self.upstream_server_name
            .try_read()
            .ok()
            .and_then(|g| g.clone())
    }

    fn configured_server_type(&self) -> Option<String> {
        effective_server_type(self.config.server_type_override.clone(), None)
            .map(|s| s.to_lowercase())
    }

    fn subscribe_tools_changed(&self) -> Option<broadcast::Receiver<()>> {
        Some(self.tools_changed_tx.subscribe())
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        async {
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
        .instrument(self.span.clone())
        .await
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
            /// When true, /message answers `server/discover` with a `2026-07-28`
            /// result (used to test the 2026 stateless version-gating path).
            discover_returns_2026: AtomicBool,
            /// Bodies of every POST /message received, in arrival order.
            posts: std::sync::Mutex<Vec<Value>>,
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
        ) -> axum::response::Response {
            app.fake.posts.lock().unwrap().push(body.clone());

            let method = body["method"].as_str().unwrap_or("").to_string();

            // Notifications have no `id` and produce no response body.
            if body.get("id").is_none() {
                return (StatusCode::ACCEPTED, "").into_response();
            }

            let id = body["id"].as_u64().unwrap_or(0);
            let response = match method.as_str() {
                "server/discover" if app.fake.discover_returns_2026.load(Ordering::SeqCst) => {
                    json!({
                        "jsonrpc": "2.0",
                        "result": {
                            "protocolVersion": "2026-07-28",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fake-sse-2026", "version": "1.0.0"}
                        },
                        "id": id,
                    })
                }
                "initialize" => json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-sse", "version": "0.0.0"}
                    },
                    "id": id,
                }),
                "tools/list" => json!({
                    "jsonrpc": "2.0",
                    "result": {"tools": []},
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
            Json(response).into_response()
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
        async fn test_sse_sends_initialized_notification_after_handshake() {
            let server = start_test_server().await;
            let mut adapter = build_adapter(&server.url, 5).await;

            adapter.initialize().await.unwrap();

            let posts = server.state.posts.lock().unwrap().clone();
            assert!(
                posts.len() >= 2,
                "expected at least 2 POSTs (initialize + notifications/initialized), got {}: {:?}",
                posts.len(),
                posts
            );

            let init_idx = posts
                .iter()
                .position(|b| b.get("method").and_then(|m| m.as_str()) == Some("initialize"))
                .expect("initialize POST should be recorded");
            let notif_idx = posts
                .iter()
                .position(|b| {
                    b.get("method").and_then(|m| m.as_str()) == Some("notifications/initialized")
                })
                .expect("notifications/initialized POST should be recorded");
            assert!(
                notif_idx > init_idx,
                "notifications/initialized must come after initialize; init_idx={}, notif_idx={}",
                init_idx,
                notif_idx
            );

            let notif = &posts[notif_idx];
            assert!(
                notif.get("id").is_none(),
                "notifications/initialized must not carry an id field, got {:?}",
                notif
            );
            // No other request methods should have been POSTed between
            // initialize and notifications/initialized.
            for (i, b) in posts.iter().enumerate() {
                if i > init_idx && i < notif_idx {
                    panic!(
                        "unexpected POST between initialize and notifications/initialized: {:?}",
                        b
                    );
                }
            }

            adapter.shutdown().await.unwrap();
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

        #[test]
        fn test_inject_client_info_creates_and_preserves_params() {
            // None params → a fresh object carrying only `_meta` clientInfo.
            let injected = SseAdapter::inject_client_info(None).unwrap();
            let ci = &injected["_meta"][protocol::META_CLIENT_INFO_KEY];
            assert_eq!(ci["name"], "endara-relay");
            assert!(ci["version"].is_string());

            // Existing fields are preserved; `_meta` clientInfo is added.
            let injected =
                SseAdapter::inject_client_info(Some(json!({"name": "echo", "arguments": {}})))
                    .unwrap();
            assert_eq!(injected["name"], "echo");
            assert_eq!(
                injected["_meta"][protocol::META_CLIENT_INFO_KEY]["name"],
                "endara-relay"
            );

            // A pre-existing OBJECT `_meta` with sibling keys (e.g. W3C Trace
            // Context) is preserved; clientInfo is added alongside the siblings.
            let injected = SseAdapter::inject_client_info(Some(json!({
                "name": "echo",
                "_meta": {"traceparent": "tp", "tracestate": "ts"}
            })))
            .unwrap();
            assert_eq!(injected["_meta"]["traceparent"], "tp");
            assert_eq!(injected["_meta"]["tracestate"], "ts");
            assert_eq!(
                injected["_meta"][protocol::META_CLIENT_INFO_KEY]["name"],
                "endara-relay"
            );

            // A pre-existing NON-OBJECT `_meta` (here a String) must NOT panic:
            // it is normalized to an object and clientInfo is still injected.
            let injected = SseAdapter::inject_client_info(Some(
                json!({"name": "echo", "_meta": "not-an-object"}),
            ))
            .unwrap();
            assert!(injected["_meta"].is_object());
            assert_eq!(
                injected["_meta"][protocol::META_CLIENT_INFO_KEY]["name"],
                "endara-relay"
            );
        }

        /// 2026 upstream over SSE: the `server/discover` probe detects
        /// `2026-07-28`, so the adapter skips `initialize`/
        /// `notifications/initialized` entirely, and every subsequent POST
        /// carries `_meta` clientInfo (SSE has no per-request identity headers).
        #[tokio::test]
        async fn test_sse_2026_path_skips_handshake_and_injects_meta() {
            let server = start_test_server().await;
            server
                .state
                .discover_returns_2026
                .store(true, Ordering::SeqCst);

            let mut adapter = build_adapter(&server.url, 3).await;
            adapter
                .initialize()
                .await
                .expect("2026 initialize succeeds");
            assert!(
                adapter.upstream_dialect().await.is_2026(),
                "upstream should be detected as 2026"
            );
            let _ = adapter.list_tools().await.expect("list_tools");
            adapter.shutdown().await.unwrap();

            let posts = server.state.posts.lock().unwrap().clone();
            let methods: Vec<&str> = posts.iter().filter_map(|p| p["method"].as_str()).collect();
            assert!(
                methods.contains(&"server/discover"),
                "discover probe must be sent, got {methods:?}"
            );
            assert!(
                !methods.contains(&"initialize"),
                "2026 path must skip initialize, got {methods:?}"
            );
            assert!(
                !methods.contains(&"notifications/initialized"),
                "2026 path must skip notifications/initialized, got {methods:?}"
            );
            for p in &posts {
                assert_eq!(
                    p["params"]["_meta"][protocol::META_CLIENT_INFO_KEY]["name"].as_str(),
                    Some("endara-relay"),
                    "every 2026 POST must carry _meta clientInfo, got: {p:?}"
                );
            }
        }

        /// Legacy upstream over SSE: the `server/discover` probe returns a
        /// non-2026 result, so the full `initialize`/`notifications/initialized`
        /// handshake runs and the relay injects no `_meta` clientInfo on the
        /// handshake frames.
        #[tokio::test]
        async fn test_sse_legacy_path_runs_handshake_without_meta() {
            let server = start_test_server().await;
            let mut adapter = build_adapter(&server.url, 3).await;
            adapter
                .initialize()
                .await
                .expect("legacy initialize succeeds");
            assert!(
                !adapter.upstream_dialect().await.is_2026(),
                "upstream should be detected as legacy"
            );
            adapter.shutdown().await.unwrap();

            let posts = server.state.posts.lock().unwrap().clone();
            let methods: Vec<&str> = posts.iter().filter_map(|p| p["method"].as_str()).collect();
            assert!(
                methods.contains(&"server/discover"),
                "discover probe must precede the legacy handshake, got {methods:?}"
            );
            assert!(
                methods.contains(&"initialize"),
                "legacy path must run initialize, got {methods:?}"
            );
            assert!(
                methods.contains(&"notifications/initialized"),
                "legacy path must send notifications/initialized, got {methods:?}"
            );
            // Only the discover probe carries `_meta` (it is sent before the
            // dialect is known); the legacy handshake frames must not.
            for p in &posts {
                if p["method"].as_str() == Some("server/discover") {
                    continue;
                }
                assert!(
                    p["params"].get("_meta").is_none(),
                    "legacy frame must not carry _meta, got: {p:?}"
                );
            }
        }
    }
}
