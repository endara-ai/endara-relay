use super::server_name::{sanitize_server_name, ServerNameError};
use super::server_type_resolution::{effective_server_type, strip_mcp_server_suffix};
use super::stdio::{iso8601_now, RingBuffer};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::events::{annotations_from_value, ToolCallEvent, ToolCallEventBus};
use crate::jsonrpc::{self, JsonRpcResponse};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, trace, warn, Instrument};

/// Configuration for the HTTP MCP adapter.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The URL of the HTTP MCP server endpoint (e.g., http://host:port/mcp).
    pub url: String,
    /// Request timeout in seconds (default: 30).
    pub timeout_secs: u64,
    /// Custom HTTP headers to include in every request.
    pub headers: HashMap<String, String>,
    /// Optional override for the advertised `server_type` name. See
    /// [`crate::adapter::server_type_resolution::effective_server_type`].
    pub server_type_override: Option<String>,
    /// Endpoint name (used as the `endpoint` field on the adapter's
    /// per-endpoint `tracing` span). Defaults to empty for direct test
    /// construction; production paths set this from `EndpointConfig::name`.
    pub endpoint_name: String,
}

impl HttpConfig {
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

/// HTTP MCP adapter — sends JSON-RPC requests as HTTP POST.
pub struct HttpAdapter {
    config: HttpConfig,
    client: Client,
    health: Arc<RwLock<HealthStatus>>,
    request_id: AtomicU64,
    /// Sanitized server name from the MCP initialize response.
    server_type: Arc<RwLock<Option<String>>>,
    /// Upstream-derived server name (sanitized + suffix-stripped), captured
    /// before any `server_type_override` resolution. Surfaced via
    /// [`McpAdapter::upstream_server_name`] so the management API can show the
    /// default name the upstream reports.
    upstream_server_name: Arc<RwLock<Option<String>>>,
    /// Ring buffer recording tool call activity.
    activity_log: Arc<RwLock<RingBuffer>>,
    /// Per-endpoint tracing span. Every adapter method instruments its async
    /// body with this span so events carry `endpoint`/`transport` (and
    /// `server_type` once the MCP handshake completes).
    span: tracing::Span,
    /// Broadcast emitter for `notifications/tools/list_changed` events
    /// observed from the upstream server. Ticks come from two sources:
    ///
    ///   1. The background `GET <url>` SSE listener spawned during
    ///      [`HttpAdapter::initialize`] (the Streamable HTTP transport's
    ///      "server-initiated stream" channel).
    ///   2. Inline notifications mixed into a POST response's SSE body,
    ///      dispatched by [`HttpAdapter::parse_sse_response`].
    ///
    /// Either path is sufficient; the spec allows servers to use either or
    /// both, so the adapter wires both unconditionally. Each tick is an
    /// opaque cache-invalidation signal consumed by the registry.
    tools_changed_tx: broadcast::Sender<()>,
    /// Handle to the background `GET <url>` SSE listener task spawned during
    /// [`HttpAdapter::initialize`]. Aborted on [`HttpAdapter::shutdown`] and
    /// when the adapter is dropped so the task never outlives the adapter.
    listener_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Signaled by [`HttpAdapter::shutdown`] (and by [`Drop`]) so the GET
    /// listener loop exits cleanly between SSE reads and reconnect backoffs
    /// instead of waiting out the current sleep / network read.
    shutdown_notify: Arc<Notify>,
    /// Shared typed event bus for the desktop overlay's SSE stream. See the
    /// matching field on [`super::stdio::StdioAdapter`].
    event_bus: Arc<OnceLock<ToolCallEventBus>>,
    /// Per-tool annotation cache populated from `list_tools()` responses so
    /// `call_tool` can attach hint metadata to the overlay's `started`
    /// event without a second round-trip.
    tool_annotations_cache: Arc<RwLock<HashMap<String, Option<Value>>>>,
}

impl HttpAdapter {
    /// Create a new HttpAdapter with the given configuration.
    pub fn new(config: HttpConfig) -> Self {
        let mut default_headers = reqwest::header::HeaderMap::new();
        // The Streamable HTTP transport spec requires clients to accept both
        // application/json and text/event-stream.  Set this before processing
        // user headers so it is always present.
        default_headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json, text/event-stream"),
        );

        for (key, value) in &config.headers {
            if key.eq_ignore_ascii_case("content-type") {
                warn!(header = %key, "Ignoring custom Content-Type header; JSON-RPC requires application/json");
                continue;
            }
            if key.eq_ignore_ascii_case("accept") {
                warn!(header = %key, "Ignoring custom Accept header; Streamable HTTP transport requires application/json, text/event-stream");
                continue;
            }
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                default_headers.insert(name, val);
            } else {
                warn!(header = %key, "Invalid header name or value, skipping");
            }
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .default_headers(default_headers)
            .build()
            .expect("failed to build HTTP client");

        let span = tracing::info_span!(
            "endpoint",
            endpoint = %config.endpoint_name,
            transport = "http",
            server_type = tracing::field::Empty,
        );
        let (tools_changed_tx, _) = broadcast::channel(16);
        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
            server_type: Arc::new(RwLock::new(None)),
            upstream_server_name: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
            span,
            tools_changed_tx,
            listener_handle: Arc::new(Mutex::new(None)),
            shutdown_notify: Arc::new(Notify::new()),
            event_bus: Arc::new(OnceLock::new()),
            tool_annotations_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new HttpAdapter with a pre-built reqwest::Client.
    ///
    /// Top-level constructor kept for completeness; OAuth wrapping uses
    /// [`HttpAdapter::new_with_client_inner`] instead so the inner adapter
    /// does not create its own `endpoint` tracing span.
    #[allow(dead_code)]
    pub fn new_with_client(config: HttpConfig, client: Client) -> Self {
        let span = tracing::info_span!(
            "endpoint",
            endpoint = %config.endpoint_name,
            transport = "http",
            server_type = tracing::field::Empty,
        );
        Self::with_span(config, client, span)
    }

    /// Create a new HttpAdapter intended to be wrapped by another adapter
    /// (currently `OAuthAdapter`) that already owns the per-endpoint
    /// `endpoint` tracing span. The inner adapter uses `tracing::Span::none()`
    /// so its `.instrument(self.span.clone())` calls become no-ops and events
    /// are attached to the enclosing wrapper's span instead, avoiding a
    /// duplicated `endpoint=<name>` field.
    pub fn new_with_client_inner(config: HttpConfig, client: Client) -> Self {
        Self::with_span(config, client, tracing::Span::none())
    }

    fn with_span(config: HttpConfig, client: Client, span: tracing::Span) -> Self {
        let (tools_changed_tx, _) = broadcast::channel(16);
        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
            server_type: Arc::new(RwLock::new(None)),
            upstream_server_name: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
            span,
            tools_changed_tx,
            listener_handle: Arc::new(Mutex::new(None)),
            shutdown_notify: Arc::new(Notify::new()),
            event_bus: Arc::new(OnceLock::new()),
            tool_annotations_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Install the given event-bus handle (Arc-cloned) on this adapter,
    /// replacing the slot reserved by the constructor. Used by
    /// [`crate::adapter::oauth::OAuthAdapter`] to share a single
    /// `OnceLock` cell across every inner adapter it rebuilds.
    pub(crate) fn set_event_bus_handle(&mut self, handle: Arc<OnceLock<ToolCallEventBus>>) {
        self.event_bus = handle;
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Parse an SSE (text/event-stream) response body and extract the JSON-RPC
    /// response matching the given request `id`.
    ///
    /// SSE events are separated by double newlines.  Each event may contain
    /// `data:` lines whose payloads are concatenated (with newline separators)
    /// to form the event data.  We look for the first event whose data
    /// deserialises to a `JsonRpcResponse` with a matching `id`.
    ///
    /// When `tools_changed_tx` is `Some`, any event in the stream that decodes
    /// as a JSON-RPC notification (i.e. no `id`) with `method ==
    /// "notifications/tools/list_changed"` is dispatched as a tick on that
    /// broadcast. This is how POST inline notifications (the Streamable HTTP
    /// spec's "mix notifications into a POST SSE response" path) reach the
    /// registry alongside the long-lived `GET` listener.
    fn parse_sse_response(
        body: &str,
        id: u64,
        tools_changed_tx: Option<&broadcast::Sender<()>>,
    ) -> Result<JsonRpcResponse, AdapterError> {
        let mut matched: Option<JsonRpcResponse> = None;
        for event in body.split("\n\n") {
            let event = event.trim();
            if event.is_empty() {
                continue;
            }

            // Collect all `data:` lines for this event and concatenate them.
            let mut data_parts: Vec<&str> = Vec::new();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.strip_prefix(' ').unwrap_or(data);
                    if !data.is_empty() {
                        data_parts.push(data);
                    }
                }
            }

            if data_parts.is_empty() {
                continue;
            }

            let data = data_parts.join("\n");

            // Dispatch tools-changed notifications inline as we walk the
            // stream so they aren't lost when a POST SSE body carries both a
            // response and a notification (Streamable HTTP spec allows this).
            // We only check `tools_changed_tx` when it's provided — pure-parse
            // callers (tests) pass `None` and the dispatch is a no-op.
            if let Some(tx) = tools_changed_tx {
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    if value.get("id").is_none() {
                        if let Some("notifications/tools/list_changed") =
                            value.get("method").and_then(|m| m.as_str())
                        {
                            debug!(
                                "received tools/list_changed notification inline with POST SSE response"
                            );
                            let _ = tx.send(());
                        }
                    }
                }
            }

            if matched.is_none() {
                if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&data) {
                    // Match on id — notifications (id == None) are skipped.
                    if response.id == Some(id) {
                        matched = Some(response);
                    }
                }
            }
        }

        if let Some(resp) = matched {
            return Ok(resp);
        }

        Err(AdapterError::ProtocolError(
            "no matching JSON-RPC response found in SSE stream".into(),
        ))
    }

    /// Send a JSON-RPC notification via HTTP POST.
    ///
    /// Notifications are JSON-RPC messages without an `id` field.  Per the MCP
    /// Streamable HTTP spec the server responds with 202 Accepted and an empty
    /// body.  We therefore do **not** attempt to parse a JSON-RPC response.
    async fn send_notification(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), AdapterError> {
        let mut request = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(p) = params {
            request["params"] = p;
        }

        trace!(method = method, url = %self.config.url, "sending HTTP JSON-RPC notification");

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
                    AdapterError::HttpError {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = resp.status();
        // 202 Accepted is the expected response for notifications.
        // Some servers may return 200 OK — accept that too.
        if status == reqwest::StatusCode::ACCEPTED || status.is_success() {
            trace!(method = method, status = %status, "notification accepted");
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(AdapterError::HttpError {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Send a JSON-RPC request via HTTP POST and return the result.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let id = self.next_id();
        let request = jsonrpc::new_request(method, params, id);

        trace!(method = method, id = id, url = %self.config.url, "sending HTTP JSON-RPC request");

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
                    AdapterError::HttpError {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AdapterError::HttpError {
                status: status.as_u16(),
                body,
            });
        }

        // The Streamable HTTP transport spec allows servers to respond with
        // either application/json (single JSON-RPC response) or
        // text/event-stream (SSE containing one or more JSON-RPC messages).
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let response: JsonRpcResponse = if content_type.contains("text/event-stream") {
            trace!(
                id = id,
                "response is SSE (text/event-stream), parsing events"
            );
            let body = resp.text().await.map_err(|e| {
                AdapterError::ProtocolError(format!("failed to read SSE body: {}", e))
            })?;
            Self::parse_sse_response(&body, id, Some(&self.tools_changed_tx))?
        } else {
            resp.json().await.map_err(|e| {
                AdapterError::ProtocolError(format!("invalid JSON-RPC response: {}", e))
            })?
        };

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

    /// Long-lived `GET <url>` SSE listener body. Streamable HTTP transport
    /// allows servers to push server-initiated messages (including
    /// `notifications/tools/list_changed`) via a GET request that opens an
    /// SSE channel separate from the request/response POST path.
    ///
    /// On any of: transport error, non-2xx response (notably 404/405 from
    /// servers that don't implement the GET stream), or shutdown signal, the
    /// task exits quietly — inline POST notifications still reach the
    /// broadcast via [`HttpAdapter::parse_sse_response`].
    async fn run_get_listener(
        url: String,
        headers: HashMap<String, String>,
        tools_changed_tx: broadcast::Sender<()>,
        shutdown: Arc<Notify>,
    ) {
        // Separate client for the long-lived stream — the per-request timeout
        // on the main client (30s by default) would tear down the stream.
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("text/event-stream"),
        );
        for (key, value) in &headers {
            if key.eq_ignore_ascii_case("accept") || key.eq_ignore_ascii_case("content-type") {
                continue;
            }
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                default_headers.insert(name, val);
            }
        }
        let client = match Client::builder().default_headers(default_headers).build() {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "GET listener: failed to build HTTP client; exiting");
                return;
            }
        };

        let resp = tokio::select! {
            _ = shutdown.notified() => return,
            r = client.get(&url).header(reqwest::header::ACCEPT, "text/event-stream").send() => match r {
                Ok(r) => r,
                Err(e) => {
                    debug!(error = %e, "GET listener: connect/send failed; exiting");
                    return;
                }
            }
        };

        let status = resp.status();
        if !status.is_success() {
            debug!(
                status = %status,
                "GET listener: non-2xx response (upstream likely doesn't support server-initiated streams); exiting"
            );
            return;
        }

        use futures_util::StreamExt;
        let mut bytes_stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut data_lines: Vec<String> = Vec::new();

        loop {
            let chunk_result = tokio::select! {
                _ = shutdown.notified() => {
                    debug!("GET listener: shutdown requested; exiting");
                    return;
                }
                next = bytes_stream.next() => match next {
                    Some(r) => r,
                    None => {
                        debug!("GET listener: upstream stream ended; exiting");
                        return;
                    }
                }
            };

            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    debug!(error = %e, "GET listener: stream error; exiting");
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                buffer.drain(..=newline_pos);

                if line.is_empty() {
                    if !data_lines.is_empty() {
                        let data = data_lines.join("\n");
                        data_lines.clear();
                        if let Ok(value) = serde_json::from_str::<Value>(&data) {
                            if value.get("id").is_none()
                                && value.get("method").and_then(|m| m.as_str())
                                    == Some("notifications/tools/list_changed")
                            {
                                debug!("GET listener: received tools/list_changed notification");
                                let _ = tools_changed_tx.send(());
                            }
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("data:") {
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    data_lines.push(rest.to_string());
                }
                // Other SSE fields (event:, id:, retry:, comments) are ignored.
            }
        }
    }
}

impl Drop for HttpAdapter {
    fn drop(&mut self) {
        // Wake any pending `shutdown.notified()` in the GET listener so it
        // exits at the next `tokio::select!` tick. Drop is synchronous, so we
        // can't `.await` the handle; a best-effort `try_lock` lets us abort
        // the join handle immediately when uncontended, otherwise we rely on
        // the notification alone.
        self.shutdown_notify.notify_waiters();
        if let Ok(mut guard) = self.listener_handle.try_lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

#[async_trait]
impl McpAdapter for HttpAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        async {
            *self.health.write().await = HealthStatus::Starting;

            let params = json!({
                "protocolVersion": "2025-03-26",
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
                    error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                    return Err(e);
                }
            };

            // Extract serverInfo.name — REQUIRED per MCP spec enforcement
            let raw_name = match result
                .get("serverInfo")
                .and_then(|si| si.get("name"))
                .and_then(|n| n.as_str())
            {
                Some(name) => name,
                None => {
                    let err = ServerNameError::Missing;
                    let msg = err.to_string();
                    error!(url = %self.config.url, error = %msg, "MCP server did not provide serverInfo.name");
                    *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                    return Err(AdapterError::ProtocolError(msg));
                }
            };

            // Validate and sanitize the server name
            let sanitized = match sanitize_server_name(raw_name) {
                Ok(s) => s,
                Err(e) => {
                    let msg = e.to_string();
                    error!(url = %self.config.url, raw_name = %raw_name, error = %msg, "serverInfo.name validation failed");
                    *self.health.write().await = HealthStatus::Unhealthy(msg.clone());
                    return Err(AdapterError::ProtocolError(msg));
                }
            };

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

            info!(url = %self.config.url, raw_name = %raw_name, sanitized = %sanitized, effective = ?effective, "MCP server reported serverInfo.name");
            if let Some(ref name) = effective {
                self.span.record("server_type", tracing::field::display(name));
            }
            *self.server_type.write().await = effective;
            *self.upstream_server_name.write().await = Some(upstream_stripped);

            // Per the MCP spec the client MUST send a notifications/initialized
            // notification after a successful initialize exchange.
            if let Err(e) = self
                .send_notification("notifications/initialized", None)
                .await
            {
                warn!(url = %self.config.url, error = %e, "failed to send notifications/initialized");
            }

            // Spawn the long-lived `GET <url>` SSE listener. Streamable HTTP
            // servers may deliver server-initiated notifications (notably
            // `notifications/tools/list_changed`) via this channel. Upstreams
            // that don't support it return 404/405 and the task exits
            // quietly — inline POST notifications still reach the broadcast
            // via `parse_sse_response`.
            let url = self.config.url.clone();
            let headers = self.config.headers.clone();
            let tx = self.tools_changed_tx.clone();
            let shutdown = self.shutdown_notify.clone();
            let listener_span = self.span.clone();
            let handle = tokio::spawn(
                async move {
                    Self::run_get_listener(url, headers, tx, shutdown).await;
                }
                .instrument(listener_span),
            );
            *self.listener_handle.lock().await = Some(handle);

            *self.health.write().await = HealthStatus::Healthy;
            info!(url = %self.config.url, "HTTP MCP adapter initialized");
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

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
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
                    ts: iso8601_now(),
                    endpoint: self.config.endpoint_name.clone(),
                    transport: "http".into(),
                    server_type: self.server_type.read().await.clone(),
                    server_name: self.upstream_server_name.read().await.clone(),
                    profile: None,
                    tool: name.to_string(),
                    annotations,
                });
            }
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
            match &result {
                Ok(_) => tracing::info!(
                    tool = %name,
                    status = "ok",
                    duration_ms = duration_ms,
                    "Tool call completed"
                ),
                Err(e) => tracing::warn!(
                    tool = %name,
                    status = "error",
                    duration_ms = duration_ms,
                    error = %e,
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
            // Signal the GET listener to exit at the next select tick, then
            // await its handle so it tears down before the adapter does.
            self.shutdown_notify.notify_waiters();
            if let Some(handle) = self.listener_handle.lock().await.take() {
                handle.abort();
                let _ = handle.await;
            }
            *self.health.write().await = HealthStatus::Stopped;
            info!(url = %self.config.url, "HTTP MCP adapter shut down");
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

    #[test]
    fn test_default_accept_header_present() {
        // The HttpAdapter should always set Accept: application/json, text/event-stream
        let config = HttpConfig::new("http://localhost:8080/mcp");
        let adapter = HttpAdapter::new(config);
        // We can't directly inspect default_headers on reqwest::Client, but we can
        // verify that creating the adapter with custom Accept header doesn't panic
        // and the adapter is still functional (Accept is skipped in favor of default).
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_custom_accept_header_is_skipped() {
        // User-provided Accept headers should be ignored (logged as warning).
        let mut config = HttpConfig::new("http://localhost:8080/mcp");
        config
            .headers
            .insert("Accept".to_string(), "text/html".to_string());
        // Should not panic — the custom Accept is skipped.
        let adapter = HttpAdapter::new(config);
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_custom_content_type_header_is_skipped() {
        // User-provided Content-Type headers should be ignored.
        let mut config = HttpConfig::new("http://localhost:8080/mcp");
        config
            .headers
            .insert("Content-Type".to_string(), "text/xml".to_string());
        let adapter = HttpAdapter::new(config);
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    #[test]
    fn test_custom_auth_header_is_applied() {
        // Non-restricted custom headers should be applied without issue.
        let mut config = HttpConfig::new("http://localhost:8080/mcp");
        config
            .headers
            .insert("Authorization".to_string(), "Bearer test-token".to_string());
        let adapter = HttpAdapter::new(config);
        assert_eq!(adapter.health(), HealthStatus::Stopped);
    }

    // --- SSE parsing tests ---

    #[test]
    fn test_parse_sse_simple_response() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"tools\":[]},\"id\":1}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 1, None).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_parse_sse_without_event_field() {
        // Some servers only send `data:` lines, no `event:` line.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true},\"id\":5}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 5, None).unwrap();
        assert_eq!(resp.id, Some(5));
        assert!(resp.result.is_some());
    }

    #[test]
    fn test_parse_sse_multiple_events_matches_id() {
        // First event is a notification (no id), second is the response.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"done\":true},\"id\":3}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 3, None).unwrap();
        assert_eq!(resp.id, Some(3));
    }

    #[test]
    fn test_parse_sse_no_matching_id() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{},\"id\":99}\n\n";
        let err = HttpAdapter::parse_sse_response(body, 1, None).unwrap_err();
        assert!(
            matches!(err, AdapterError::ProtocolError(_)),
            "expected ProtocolError, got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_sse_empty_body() {
        let err = HttpAdapter::parse_sse_response("", 1, None).unwrap_err();
        assert!(matches!(err, AdapterError::ProtocolError(_)));
    }

    #[test]
    fn test_parse_sse_error_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"},\"id\":2}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 2, None).unwrap();
        assert_eq!(resp.id, Some(2));
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn test_parse_sse_multiline_data_invalid_json() {
        // If multi-line data concatenation produces invalid JSON, the event is skipped.
        let body = "data: {\"incomplete\":\ndata: true}\n\n";
        let err = HttpAdapter::parse_sse_response(body, 1, None).unwrap_err();
        assert!(matches!(err, AdapterError::ProtocolError(_)));
    }

    #[test]
    fn test_parse_sse_data_no_space_after_colon() {
        // SSE spec says space after colon is optional.
        let body = "data:{\"jsonrpc\":\"2.0\",\"result\":{\"x\":1},\"id\":4}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 4, None).unwrap();
        assert_eq!(resp.id, Some(4));
    }

    #[test]
    fn test_parse_sse_ignores_non_data_lines() {
        let body = "event: message\nid: 123\nretry: 5000\ndata: {\"jsonrpc\":\"2.0\",\"result\":{},\"id\":1}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 1, None).unwrap();
        assert_eq!(resp.id, Some(1));
    }

    // --- Additional SSE parsing tests ---

    #[test]
    fn test_parse_sse_multiline_data_concatenation() {
        // Multi `data:` lines form valid JSON when joined with newlines.
        // JSON allows whitespace (incl. newlines) between tokens.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"result\":\n",
            "data: {\"tools\":[{\"name\":\"a\"}]}\n",
            "data: ,\"id\":1}\n",
            "\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 1, None).unwrap();
        assert_eq!(resp.id, Some(1));
        let tools = resp.result.unwrap();
        let arr = tools.get("tools").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "a");
    }

    #[test]
    fn test_parse_sse_todoist_style_initialize_response() {
        // Realistic MCP initialize response with serverInfo, capabilities, protocolVersion.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{\"tools\":{\"listChanged\":true}},\"serverInfo\":{\"name\":\"todoist-mcp\",\"version\":\"1.0.0\"}},\"id\":1}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 1, None).unwrap();
        assert_eq!(resp.id, Some(1));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "todoist-mcp");
        assert_eq!(result["serverInfo"]["version"], "1.0.0");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
    }

    #[test]
    fn test_parse_sse_large_tools_list_response() {
        // A tools/list response with 6 tools, each with full inputSchema.
        let tools_json = serde_json::json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [
                    {"name": "create_task", "description": "Create a new task", "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}, "priority": {"type": "integer", "minimum": 1, "maximum": 4}}, "required": ["title"]}},
                    {"name": "get_task", "description": "Get task by ID", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
                    {"name": "update_task", "description": "Update an existing task", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}, "title": {"type": "string"}, "priority": {"type": "integer"}}, "required": ["id"]}},
                    {"name": "delete_task", "description": "Delete a task", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
                    {"name": "list_tasks", "description": "List all tasks with filters", "inputSchema": {"type": "object", "properties": {"project_id": {"type": "string"}, "status": {"type": "string", "enum": ["active", "completed"]}, "limit": {"type": "integer", "default": 50}}}},
                    {"name": "search_tasks", "description": "Search tasks by query", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["query"]}}
                ]
            },
            "id": 2
        });
        let body = format!("data: {}\n\n", serde_json::to_string(&tools_json).unwrap());
        let resp = HttpAdapter::parse_sse_response(&body, 2, None).unwrap();
        assert_eq!(resp.id, Some(2));
        let tools = resp.result.unwrap();
        let arr = tools.get("tools").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 6);
        assert_eq!(arr[0]["name"], "create_task");
        assert_eq!(arr[5]["name"], "search_tasks");
        // Verify schemas are preserved
        assert_eq!(
            arr[0]["inputSchema"]["properties"]["priority"]["maximum"],
            4
        );
    }

    #[test]
    fn test_parse_sse_multiple_events_notification_error_result() {
        // 3 events: progress notification (no id), error for id=99, correct result for id=7.
        // The parser must pick the one matching id=7.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":50,\"total\":100}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32600,\"message\":\"Invalid request\"},\"id\":99}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}]},\"id\":7}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 7, None).unwrap();
        assert_eq!(resp.id, Some(7));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["content"][0]["text"], "Hello");
    }

    #[test]
    fn test_parse_sse_crlf_line_endings() {
        // Some servers/proxies use Windows-style \r\n line endings.
        // The parser should still handle this because split("\n\n") finds
        // the double-newline within \r\n\r\n, and trim() strips leftover \r.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true},\"id\":1}\r\n\r\n";
        let resp = HttpAdapter::parse_sse_response(body, 1, None).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert_eq!(resp.result.unwrap()["ok"], true);
    }

    #[test]
    fn test_parse_sse_content_type_charset_detection() {
        // Verify that content_type detection with charset parameter works.
        // send_request() uses `.contains("text/event-stream")` so
        // "text/event-stream; charset=utf-8" should still match.
        let content_type = "text/event-stream; charset=utf-8";
        assert!(
            content_type.contains("text/event-stream"),
            "charset parameter should not prevent SSE detection"
        );

        // Also test the actual SSE parsing works with a body that would come
        // from such a content type.
        let body = "data: {\"jsonrpc\":\"2.0\",\"result\":{\"encoding\":\"utf-8\"},\"id\":10}\n\n";
        let resp = HttpAdapter::parse_sse_response(body, 10, None).unwrap();
        assert_eq!(resp.id, Some(10));
    }

    #[test]
    fn test_parse_sse_trailing_whitespace_and_extra_newlines() {
        // Body with extra blank lines between events, trailing whitespace on data lines.
        let body = concat!(
            "\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}  \n",
            "\n\n",
            "\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"status\":\"ok\"},\"id\":3}   \n",
            "\n\n",
            "\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 3, None).unwrap();
        assert_eq!(resp.id, Some(3));
        assert_eq!(resp.result.unwrap()["status"], "ok");
    }

    #[test]
    fn test_parse_sse_initialized_notification_skipped_in_stream() {
        // The client now sends notifications/initialized after a successful
        // initialize exchange (see send_notification).  Verify that a
        // notifications/initialized message appearing in an SSE stream is
        // correctly skipped (it has no id) and doesn't interfere with
        // response matching.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"serverInfo\":{\"name\":\"test\",\"version\":\"0.1\"}},\"id\":1}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 1, None).unwrap();
        assert_eq!(resp.id, Some(1));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2025-03-26");
    }

    // --- tools/list_changed dispatch tests ---

    /// Inline POST path: a POST SSE response body containing both a
    /// `notifications/tools/list_changed` event AND the matching JSON-RPC
    /// response dispatches the notification to the broadcast and still
    /// returns the response.
    #[test]
    fn test_parse_sse_dispatches_inline_tools_changed_notification() {
        let (tx, mut rx) = broadcast::channel::<()>(8);
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true},\"id\":42}\n\n",
        );
        let resp = HttpAdapter::parse_sse_response(body, 42, Some(&tx)).unwrap();
        assert_eq!(resp.id, Some(42));
        assert_eq!(resp.result.unwrap()["ok"], true);
        assert!(
            rx.try_recv().is_ok(),
            "broadcast receiver should have received a tick"
        );
    }

    // --- GET listener integration tests (in-process axum server) ---

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use axum::{Json, Router};
    use std::convert::Infallible;
    use std::sync::atomic::AtomicBool;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct GetListenerAppState {
        /// When true, GET /mcp returns 405. When false, GET /mcp returns an
        /// SSE stream that emits a single `notifications/tools/list_changed`
        /// event after a short delay and keeps the connection open.
        get_returns_405: Arc<AtomicBool>,
    }

    async fn handle_get_listener(
        State(app): State<GetListenerAppState>,
    ) -> axum::response::Response {
        if app.get_returns_405.load(Ordering::SeqCst) {
            return (StatusCode::METHOD_NOT_ALLOWED, "GET not supported").into_response();
        }
        let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(8);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx
                .send(Ok(Event::default().data(
                    "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}",
                )))
                .await;
            // Keep the channel alive (and thus the connection open) by holding
            // the sender until the receiver is dropped (client disconnect).
            tx.closed().await;
        });
        Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
            .keep_alive(KeepAlive::default())
            .into_response()
    }

    async fn handle_post_initialize(Json(body): Json<Value>) -> impl IntoResponse {
        let id = body["id"].as_u64().unwrap_or(0);
        let method = body["method"].as_str().unwrap_or("");
        if method == "initialize" {
            Json(json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "fake-http", "version": "0.0.0"}
                },
                "id": id,
            }))
            .into_response()
        } else if body.get("id").is_none() {
            // notifications/initialized and other JSON-RPC notifications
            (StatusCode::ACCEPTED, "").into_response()
        } else {
            Json(json!({
                "jsonrpc": "2.0",
                "result": {"ok": true},
                "id": id,
            }))
            .into_response()
        }
    }

    async fn start_fake_http_server(
        get_returns_405: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let state = GetListenerAppState {
            get_returns_405: Arc::new(AtomicBool::new(get_returns_405)),
        };
        let app = Router::new()
            .route("/mcp", any(get_handler_dispatch))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/mcp", addr);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, handle)
    }

    async fn get_handler_dispatch(
        State(app): State<GetListenerAppState>,
        req: axum::extract::Request,
    ) -> axum::response::Response {
        if req.method() == axum::http::Method::GET {
            handle_get_listener(State(app)).await
        } else if req.method() == axum::http::Method::POST {
            let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "bad body").into_response(),
            };
            let value: Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(_) => return (StatusCode::BAD_REQUEST, "bad json").into_response(),
            };
            handle_post_initialize(Json(value)).await.into_response()
        } else {
            (StatusCode::METHOD_NOT_ALLOWED, "").into_response()
        }
    }

    /// GET listener happy path: server emits an SSE event containing
    /// `notifications/tools/list_changed`; the broadcast receiver gets a tick
    /// within 2s. Dropping the adapter causes the listener task to exit.
    #[tokio::test]
    async fn test_get_listener_dispatches_tools_changed_and_exits_on_drop() {
        let (url, server) = start_fake_http_server(false).await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");
        adapter.initialize().await.expect("initialize succeeds");

        let tick = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(tick.is_ok(), "broadcast should receive a tick within 2s");
        assert!(tick.unwrap().is_ok(), "tick should not be a lag/closed err");

        // Grab the listener handle before drop so we can assert it stops.
        let listener = {
            let mut guard = adapter.listener_handle.lock().await;
            guard
                .take()
                .expect("listener handle present after initialize")
        };
        drop(adapter);
        // Give the listener a moment to observe the shutdown notification.
        for _ in 0..20 {
            if listener.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            listener.is_finished(),
            "GET listener should exit after adapter is dropped"
        );
        server.abort();
    }

    /// GET 405 fallback: server replies 405 to GET; the listener task exits
    /// without panicking, and the inline POST path still works for a
    /// subsequent `send_request`.
    #[tokio::test]
    async fn test_get_listener_405_fallback_keeps_post_working() {
        let (url, server) = start_fake_http_server(true).await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");

        // Listener should observe the 405 and exit quickly.
        let listener = {
            let mut guard = adapter.listener_handle.lock().await;
            guard
                .take()
                .expect("listener handle present after initialize")
        };
        for _ in 0..20 {
            if listener.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            listener.is_finished(),
            "GET listener should exit promptly on 405"
        );

        // Inline POST still works.
        let result = adapter
            .send_request("tools/call", Some(json!({"name": "x"})))
            .await
            .expect("POST send_request still works after GET 405");
        assert_eq!(result["ok"], true);

        server.abort();
    }
}
