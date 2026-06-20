use super::oauth::jit::{self, JitInterceptor};
use super::server_name::{sanitize_server_name, ServerNameError};
use super::server_type_resolution::{effective_server_type, strip_mcp_server_suffix};
use super::stdio::{iso8601_now, RingBuffer};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
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
    /// Current `Mcp-Session-Id` (per the Streamable HTTP transport spec).
    /// Populated by [`HttpAdapter::initialize`] from the initialize response
    /// header (if the upstream sent one) and echoed back on every subsequent
    /// POST and on the long-lived `GET <url>` listener. Stays `None` for
    /// upstreams that don't issue a session ID — those servers continue to
    /// work without the header.
    session_id: Arc<RwLock<Option<String>>>,
    /// Optional just-in-time OAuth interceptor. `None` for every adapter built
    /// today, so the call path is behaviorally unchanged. When attached (by
    /// follow-up task 098e0e03), a hard `HTTP 401` + `WWW-Authenticate: Bearer`
    /// on a tool call is swallowed and self-initiates the OAuth flow instead of
    /// being forwarded downstream.
    jit_interceptor: Option<Arc<JitInterceptor>>,
    /// Most recent `WWW-Authenticate` header observed on a `401` response,
    /// captured before the response body is consumed so [`Self::call_tool`] can
    /// hand it to the JIT interceptor. Per-host challenges are effectively
    /// constant, so a concurrent overwrite is harmless.
    last_www_authenticate: Arc<RwLock<Option<String>>>,
    /// Negotiated protocol dialect of the upstream server. Defaults to the
    /// legacy `2025-03-26` version this adapter advertises in `initialize`;
    /// real negotiation populates it via [`Self::set_upstream_dialect`] (T7).
    /// Consumed by the 2026 outbound code paths (T8).
    upstream_dialect: Arc<RwLock<ProtocolVersion>>,
}

/// HTTP header name reqwest reads/writes for the MCP session ID. Reqwest's
/// `HeaderMap` stores names lowercase internally, so reading and writing both
/// go through this constant. The wire-level spelling stays `Mcp-Session-Id`
/// per the spec; HTTP header names are case-insensitive on transmit.
const MCP_SESSION_ID_HEADER: reqwest::header::HeaderName =
    reqwest::header::HeaderName::from_static("mcp-session-id");

/// 2026 Streamable HTTP per-request headers (lowercased for reqwest's
/// `HeaderMap`). Emitted only to upstreams detected as `2026-07-28`:
/// `MCP-Protocol-Version` conveys the dialect, `Mcp-Method` mirrors the
/// JSON-RPC method, and `Mcp-Name` mirrors the `tools/call` tool name —
/// enabling routing/observability without parsing the body.
const MCP_PROTOCOL_VERSION_HEADER_NAME: reqwest::header::HeaderName =
    reqwest::header::HeaderName::from_static(protocol::MCP_PROTOCOL_VERSION_HEADER);
const MCP_METHOD_HEADER_NAME: reqwest::header::HeaderName =
    reqwest::header::HeaderName::from_static(protocol::MCP_METHOD_HEADER);
const MCP_NAME_HEADER_NAME: reqwest::header::HeaderName =
    reqwest::header::HeaderName::from_static(protocol::MCP_NAME_HEADER);

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
            session_id: Arc::new(RwLock::new(None)),
            jit_interceptor: None,
            last_www_authenticate: Arc::new(RwLock::new(None)),
            upstream_dialect: Arc::new(RwLock::new(ProtocolVersion::V2025_03_26)),
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
            session_id: Arc::new(RwLock::new(None)),
            jit_interceptor: None,
            last_www_authenticate: Arc::new(RwLock::new(None)),
            upstream_dialect: Arc::new(RwLock::new(ProtocolVersion::V2025_03_26)),
        }
    }

    /// Install the given event-bus handle (Arc-cloned) on this adapter,
    /// replacing the slot reserved by the constructor. Used by
    /// [`crate::adapter::oauth::OAuthAdapter`] to share a single
    /// `OnceLock` cell across every inner adapter it rebuilds.
    pub(crate) fn set_event_bus_handle(&mut self, handle: Arc<OnceLock<ToolCallEventBus>>) {
        self.event_bus = handle;
    }

    /// Attach a just-in-time OAuth interceptor. Wired by follow-up task
    /// 098e0e03; unused today (every adapter is built without one).
    #[allow(dead_code)]
    pub(crate) fn set_jit_interceptor(&mut self, interceptor: Arc<JitInterceptor>) {
        self.jit_interceptor = Some(interceptor);
    }

    /// Record the upstream server's negotiated [`ProtocolVersion`]. Populated
    /// during the connection-open handshake (T7); consumed by the 2026 outbound
    /// code paths (T8).
    pub(crate) async fn set_upstream_dialect(&self, dialect: ProtocolVersion) {
        *self.upstream_dialect.write().await = dialect;
    }

    /// Read the upstream server's negotiated [`ProtocolVersion`]. Defaults to
    /// the legacy version this adapter advertises until T7 populates it.
    #[allow(dead_code)]
    pub(crate) async fn upstream_dialect(&self) -> ProtocolVersion {
        *self.upstream_dialect.read().await
    }

    /// Apply the JIT 401 interception policy to a tool-call outcome.
    ///
    /// When a JIT interceptor is attached and the upstream returned a hard
    /// `HTTP 401` (per [`jit::should_intercept_outcome`]) with a `Bearer`
    /// `WWW-Authenticate` challenge, the 401 is SWALLOWED — never forwarded
    /// downstream — and the OAuth flow is self-initiated. Otherwise the
    /// original outcome (including 200-`isError` results) is returned
    /// unchanged.
    ///
    /// On a successful self-initiation the produced authorize URL is SURFACED to
    /// the downstream client as an actionable tool result (`isError: true` with
    /// an "open this to sign in" instruction) via [`jit::surface_authorize_url`],
    /// rather than a protocol-level error — the model/CLI can act on it directly.
    ///
    /// Retry seam (chosen approach): the client re-issues the same tool call
    /// after completing the loopback `/oauth/callback`. The next call carries the
    /// now-persisted bearer (injected in [`Self::send_request`]) and succeeds.
    /// This "client re-issue" path fits the request/response adapter with the
    /// least surprise — no blocking the first call on a human-in-the-loop
    /// browser round-trip, and no hidden server-side retry state machine.
    async fn maybe_intercept_401(
        &self,
        result: Result<Value, AdapterError>,
    ) -> Result<Value, AdapterError> {
        let Some(ref interceptor) = self.jit_interceptor else {
            return result;
        };
        if !jit::should_intercept_outcome(&result) {
            return result;
        }
        let Some(challenge) = self.last_www_authenticate.read().await.clone() else {
            return result;
        };
        if jit::parse_bearer_challenge(&challenge).is_none() {
            return result;
        }
        match interceptor
            .intercept(&self.config.url, &challenge, &self.config.endpoint_name)
            .await
        {
            Ok(authorize_url) => Ok(jit::surface_authorize_url(&authorize_url)),
            Err(e) => {
                // The raw upstream 401 / WWW-Authenticate challenge must NEVER
                // reach the downstream client. When self-initiation fails we
                // still swallow the 401 and surface a sanitized, actionable
                // sign-in-unavailable result; the underlying error stays in the
                // server-side log only.
                warn!(error = %e, "JIT OAuth self-initiation failed; surfacing sanitized sign-in-unavailable result");
                Ok(jit::surface_oauth_unavailable())
            }
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// The relay's own client identity, injected under
    /// `params._meta["io.modelcontextprotocol/clientInfo"]` on every outbound
    /// request to a 2026 upstream. The 2026 transport is stateless — there is
    /// no `initialize` handshake — so identity travels per-request instead.
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
            params["_meta"][protocol::META_CLIENT_INFO_KEY] = Self::relay_client_info();
        }
        Some(params)
    }

    /// Apply the 2026 Streamable HTTP per-request headers to `builder`:
    /// `MCP-Protocol-Version` (always), `Mcp-Method` (the JSON-RPC method), and
    /// `Mcp-Name` (the `params.name` tool name, when present). These let a 2026
    /// upstream route/observe a request without parsing its body.
    fn apply_2026_headers(
        builder: reqwest::RequestBuilder,
        method: &str,
        params: Option<&Value>,
    ) -> reqwest::RequestBuilder {
        let mut builder = builder.header(
            MCP_PROTOCOL_VERSION_HEADER_NAME.clone(),
            reqwest::header::HeaderValue::from_static(protocol::VERSION_2026_07_28),
        );
        if let Ok(val) = reqwest::header::HeaderValue::from_str(method) {
            builder = builder.header(MCP_METHOD_HEADER_NAME.clone(), val);
        }
        if let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(name) {
                builder = builder.header(MCP_NAME_HEADER_NAME.clone(), val);
            }
        }
        builder
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
        // 2026 upstreams: attach `_meta` clientInfo + routing headers and omit
        // `Mcp-Session-Id` (the transport is stateless). Legacy: unchanged.
        let is_2026 = self.upstream_dialect.read().await.is_2026();
        let params = if is_2026 {
            Self::inject_client_info(params)
        } else {
            params
        };

        let mut request = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(ref p) = params {
            request["params"] = p.clone();
        }

        trace!(method = method, url = %self.config.url, "sending HTTP JSON-RPC notification");

        let mut builder = self.client.post(&self.config.url).json(&request);
        if is_2026 {
            builder = Self::apply_2026_headers(builder, method, params.as_ref());
        } else if let Some(ref id) = *self.session_id.read().await {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(id) {
                builder = builder.header(MCP_SESSION_ID_HEADER.clone(), val);
            }
        }
        let resp = builder.send().await.map_err(|e| {
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
        // 2026 upstreams: every request carries the relay's `clientInfo` under
        // `_meta` (no handshake) plus the 2026 routing headers; the stateless
        // transport replaces `Mcp-Session-Id` affinity entirely.
        let is_2026 = self.upstream_dialect.read().await.is_2026();
        let params = if is_2026 {
            Self::inject_client_info(params)
        } else {
            params
        };

        let id = self.next_id();
        let request = jsonrpc::new_request(method, params, id);

        trace!(method = method, id = id, url = %self.config.url, "sending HTTP JSON-RPC request");

        let mut builder = self.client.post(&self.config.url).json(&request);
        if is_2026 {
            builder = Self::apply_2026_headers(builder, method, request.params.as_ref());
        } else if let Some(ref sid) = *self.session_id.read().await {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(sid) {
                builder = builder.header(MCP_SESSION_ID_HEADER.clone(), val);
            }
        }
        // JIT retry-after-sign-in seam: when a JIT interceptor is attached and a
        // valid bearer has been persisted for this endpoint (after the human
        // completed the loopback `/oauth/callback`), inject it so a re-issued
        // tool call uses the held token and succeeds instead of re-triggering
        // the JIT flow. No-op when no interceptor is attached or no valid token
        // is stored, leaving the default request path unchanged.
        if let Some(ref interceptor) = self.jit_interceptor {
            if let Some(token) = interceptor.current_bearer(&self.config.endpoint_name).await {
                if let Ok(val) =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
                {
                    builder = builder.header(reqwest::header::AUTHORIZATION, val);
                }
            }
        }
        let resp = builder.send().await.map_err(|e| {
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
            // Capture the `WWW-Authenticate` challenge BEFORE consuming the
            // body so the JIT 401 interceptor (if attached) can self-initiate
            // OAuth. Only meaningful on a 401; cleared otherwise so a stale
            // challenge can't leak into a later unrelated error.
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let challenge = resp
                    .headers()
                    .get(reqwest::header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                *self.last_www_authenticate.write().await = challenge;
            } else {
                *self.last_www_authenticate.write().await = None;
            }
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
        session_id: Option<String>,
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
        if let Some(ref sid) = session_id {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(sid) {
                default_headers.insert(MCP_SESSION_ID_HEADER.clone(), val);
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

    /// Stateless `server/discover` probe used to detect a 2026 upstream before
    /// the legacy `initialize` handshake. Sent with the 2026 routing headers
    /// and `_meta` clientInfo and NO `Mcp-Session-Id`. Returns the JSON-RPC
    /// `result` object on success, or `None` on any failure (transport error,
    /// non-2xx, JSON-RPC error, missing result) so the caller falls back to the
    /// legacy handshake. Legacy servers reject `server/discover` (e.g.
    /// method-not-found) and the relay falls back transparently.
    async fn try_discover_probe(&self) -> Option<Value> {
        let id = self.next_id();
        let params = Self::inject_client_info(None);
        let request = jsonrpc::new_request("server/discover", params, id);
        trace!(method = "server/discover", id = id, url = %self.config.url, "probing upstream protocol dialect");

        let builder = self.client.post(&self.config.url).json(&request);
        let builder = Self::apply_2026_headers(builder, "server/discover", request.params.as_ref());
        let resp = builder.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let response: JsonRpcResponse = if content_type.contains("text/event-stream") {
            let body = resp.text().await.ok()?;
            Self::parse_sse_response(&body, id, Some(&self.tools_changed_tx)).ok()?
        } else {
            resp.json().await.ok()?
        };

        if response.error.is_some() {
            return None;
        }
        response.result
    }

    /// Extract, validate, and record the upstream `serverInfo.name` from an
    /// `initialize` or `server/discover` result. Sets the adapter unhealthy and
    /// returns `Err` when the name is missing or fails sanitization. Shared by
    /// the legacy handshake and the 2026 stateless paths so both name the
    /// endpoint identically.
    async fn apply_server_identity(&self, result: &Value) -> Result<(), AdapterError> {
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
            self.span
                .record("server_type", tracing::field::display(name));
        }
        *self.server_type.write().await = effective;
        *self.upstream_server_name.write().await = Some(upstream_stripped);
        Ok(())
    }

    /// Spawn the long-lived `GET <url>` SSE listener for server-initiated
    /// notifications (notably `notifications/tools/list_changed`). Snapshots the
    /// current session id at spawn time (always `None` for 2026 stateless
    /// upstreams). Shared by the legacy and 2026 initialize paths.
    async fn spawn_get_listener(&self) {
        let url = self.config.url.clone();
        let headers = self.config.headers.clone();
        // Snapshot the session ID at spawn time so the listener doesn't
        // need to re-read adapter state. Matches how `headers` is passed.
        let session_id = self.session_id.read().await.clone();
        let tx = self.tools_changed_tx.clone();
        let shutdown = self.shutdown_notify.clone();
        let listener_span = self.span.clone();
        let handle = tokio::spawn(
            async move {
                Self::run_get_listener(url, headers, session_id, tx, shutdown).await;
            }
            .instrument(listener_span),
        );
        *self.listener_handle.lock().await = Some(handle);
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

            // Discover-first dialect detection (T7/T8): probe `server/discover`
            // before the legacy handshake. A 2026 upstream answers with a
            // `protocolVersion` of `2026-07-28`, in which case the relay skips
            // the `initialize`/`notifications/initialized` handshake and the
            // `Mcp-Session-Id` machinery entirely — the 2026 transport is
            // stateless, carrying version + identity on every request instead.
            // Any other outcome (legacy result, JSON-RPC error, transport
            // failure) falls through to the unchanged legacy handshake below.
            let discover_result = self.try_discover_probe().await;
            if detect_upstream_dialect(discover_result.as_ref(), None).is_2026() {
                let result = discover_result.as_ref().expect(
                    "detect_upstream_dialect reports 2026 only when a discover result is present",
                );
                self.set_upstream_dialect(ProtocolVersion::V2026_07_28).await;
                self.apply_server_identity(result).await?;
                // 2026 is stateless: no notifications/initialized, no session id.
                self.spawn_get_listener().await;
                *self.health.write().await = HealthStatus::Healthy;
                info!(url = %self.config.url, "HTTP MCP adapter initialized (2026 stateless path)");
                return Ok(());
            }

            let params = json!({
                "protocolVersion": ProtocolVersion::V2025_03_26.as_str(),
                "capabilities": {},
                "clientInfo": {
                    "name": "endara-relay",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });

            // Inline POST for the initialize handshake so we can capture the
            // `Mcp-Session-Id` response header BEFORE the body is consumed.
            // Per the Streamable HTTP transport spec the upstream returns the
            // session ID once on the initialize response, and the client MUST
            // echo it back on every subsequent POST and on the long-lived
            // `GET <url>` listener — otherwise the server returns 400.
            //
            // Mirror `send_request`'s flow (JSON vs SSE content-type, error
            // mapping) so behaviour stays consistent with the rest of the
            // adapter's call sites.
            let result: Value = {
                let id = self.next_id();
                let request = jsonrpc::new_request("initialize", Some(params), id);
                trace!(method = "initialize", id = id, url = %self.config.url, "sending HTTP JSON-RPC request");
                let send_result = self
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
                    });
                let resp = match send_result {
                    Ok(r) => r,
                    Err(e) => {
                        let msg = e.to_string();
                        *self.health.write().await = HealthStatus::Unhealthy(msg);
                        error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                        return Err(e);
                    }
                };
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    let e = AdapterError::HttpError {
                        status: status.as_u16(),
                        body,
                    };
                    let msg = e.to_string();
                    *self.health.write().await = HealthStatus::Unhealthy(msg);
                    error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                    return Err(e);
                }

                // Capture `Mcp-Session-Id` from the initialize response BEFORE
                // we consume the body. Reqwest's `HeaderMap` lookups are
                // case-insensitive, so this matches whatever spelling the
                // upstream sends (Mcp-Session-Id, mcp-session-id, etc.).
                if let Some(sid_val) = resp.headers().get(&MCP_SESSION_ID_HEADER) {
                    if let Ok(sid_str) = sid_val.to_str() {
                        let sid_str = sid_str.trim();
                        if !sid_str.is_empty() {
                            debug!(
                                session_id = %sid_str,
                                "captured Mcp-Session-Id from initialize response"
                            );
                            *self.session_id.write().await = Some(sid_str.to_string());
                        }
                    }
                }

                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let response: JsonRpcResponse = if content_type.contains("text/event-stream") {
                    trace!(id = id, "response is SSE (text/event-stream), parsing events");
                    let body = match resp.text().await {
                        Ok(b) => b,
                        Err(e) => {
                            let err = AdapterError::ProtocolError(format!(
                                "failed to read SSE body: {}",
                                e
                            ));
                            let msg = err.to_string();
                            *self.health.write().await = HealthStatus::Unhealthy(msg);
                            error!(url = %self.config.url, error = %err, "HTTP MCP adapter initialization failed");
                            return Err(err);
                        }
                    };
                    match Self::parse_sse_response(&body, id, Some(&self.tools_changed_tx)) {
                        Ok(r) => r,
                        Err(e) => {
                            let msg = e.to_string();
                            *self.health.write().await = HealthStatus::Unhealthy(msg);
                            error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                            return Err(e);
                        }
                    }
                } else {
                    match resp.json().await {
                        Ok(r) => r,
                        Err(e) => {
                            let err = AdapterError::ProtocolError(format!(
                                "invalid JSON-RPC response: {}",
                                e
                            ));
                            let msg = err.to_string();
                            *self.health.write().await = HealthStatus::Unhealthy(msg);
                            error!(url = %self.config.url, error = %err, "HTTP MCP adapter initialization failed");
                            return Err(err);
                        }
                    }
                };

                if let Some(err) = response.error {
                    let e = AdapterError::JsonRpcError {
                        code: err.code,
                        message: err.message,
                        data: err.data,
                    };
                    let msg = e.to_string();
                    *self.health.write().await = HealthStatus::Unhealthy(msg);
                    error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
                    return Err(e);
                }

                match response.result {
                    Some(v) => v,
                    None => {
                        let err =
                            AdapterError::ProtocolError("response has no result".into());
                        let msg = err.to_string();
                        *self.health.write().await = HealthStatus::Unhealthy(msg);
                        error!(url = %self.config.url, error = %err, "HTTP MCP adapter initialization failed");
                        return Err(err);
                    }
                }
            };

            // Validate + record the upstream serverInfo.name (REQUIRED per MCP
            // spec enforcement). Shared with the 2026 stateless path above.
            self.apply_server_identity(&result).await?;

            // Record the upstream's negotiated dialect. The discover probe ran
            // above (legacy result or none) and the initialize result carries
            // the negotiated legacy version; neither is 2026 on this path.
            self.set_upstream_dialect(detect_upstream_dialect(
                discover_result.as_ref(),
                Some(&result),
            ))
            .await;

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
            self.spawn_get_listener().await;

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
                    transport: "http".into(),
                    server_type: self.server_type.read().await.clone(),
                    server_name: self.upstream_server_name.read().await.clone(),
                    profile: span_ctx.profile.clone(),
                    tool: name.to_string(),
                    annotations,
                    client: span_ctx.client.clone(),
                });
            }
            let params = json!({
                "name": name,
                "arguments": arguments,
            });
            let start = Instant::now();
            let result = self.send_request("tools/call", Some(params)).await;
            // JIT 401 interception: swallow a hard 401 + Bearer challenge and
            // self-initiate OAuth instead of forwarding it downstream. No-op
            // unless a JIT interceptor is attached (none are today).
            let result = self.maybe_intercept_401(result).await;
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

    /// Session-ID value the fake server returns on the initialize response
    /// when `require_session_id` is enabled.
    const FAKE_SESSION_ID: &str = "test-session-abc";

    #[derive(Clone)]
    struct GetListenerAppState {
        /// When true, GET /mcp returns 405. When false, GET /mcp returns an
        /// SSE stream that emits a single `notifications/tools/list_changed`
        /// event after a short delay and keeps the connection open.
        get_returns_405: Arc<AtomicBool>,
        /// When true, the initialize POST response carries
        /// `Mcp-Session-Id: test-session-abc` and every subsequent POST that
        /// arrives without a matching `Mcp-Session-Id` request header is
        /// rejected with 400 (mirroring the upstream Atlassian behaviour).
        /// Defaults to false so existing GET-listener tests stay unaffected.
        require_session_id: Arc<AtomicBool>,
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

    /// Build a POST response.
    ///
    /// `session_header` is the value of the inbound `Mcp-Session-Id` request
    /// header (if any); the handler uses it to enforce the Streamable HTTP
    /// session-ID contract when `require_session_id` is true.
    fn build_post_response(
        body: Value,
        require_session_id: bool,
        session_header: Option<String>,
    ) -> axum::response::Response {
        let id = body["id"].as_u64().unwrap_or(0);
        let method = body["method"].as_str().unwrap_or("");
        if method == "initialize" {
            let mut resp = Json(json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "fake-http", "version": "0.0.0"}
                },
                "id": id,
            }))
            .into_response();
            if require_session_id {
                resp.headers_mut().insert(
                    MCP_SESSION_ID_HEADER.clone(),
                    reqwest::header::HeaderValue::from_static(FAKE_SESSION_ID),
                );
            }
            return resp;
        }

        // For every non-initialize POST: when the server requires a session
        // ID, missing/mismatched headers get 400 with the exact spec-defined
        // error body (this is the behaviour Atlassian's MCP returns).
        if require_session_id {
            let matches = session_header
                .as_deref()
                .map(|v| v == FAKE_SESSION_ID)
                .unwrap_or(false);
            if !matches {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "error": {
                            "code": -32600,
                            "message": "Request must be an initialize request if no session ID is provided."
                        }
                    })),
                )
                    .into_response();
            }
        }

        if body.get("id").is_none() {
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

    /// Existing helper kept as a thin wrapper so the two pre-existing
    /// GET-listener tests stay unchanged.
    async fn start_fake_http_server(
        get_returns_405: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        start_fake_http_server_with_options(get_returns_405, false).await
    }

    async fn start_fake_http_server_with_options(
        get_returns_405: bool,
        require_session_id: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let state = GetListenerAppState {
            get_returns_405: Arc::new(AtomicBool::new(get_returns_405)),
            require_session_id: Arc::new(AtomicBool::new(require_session_id)),
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
            // Extract the inbound `Mcp-Session-Id` request header (axum's
            // `HeaderMap` lookups are case-insensitive) BEFORE consuming the
            // body so the validation branch can compare against it.
            let session_header = req
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let require_session_id = app.require_session_id.load(Ordering::SeqCst);
            let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
                Ok(b) => b,
                Err(_) => return (StatusCode::BAD_REQUEST, "bad body").into_response(),
            };
            let value: Value = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(_) => return (StatusCode::BAD_REQUEST, "bad json").into_response(),
            };
            build_post_response(value, require_session_id, session_header)
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

    /// Session-ID capture + replay: server returns `Mcp-Session-Id` on
    /// initialize and rejects every subsequent POST that arrives without it
    /// with 400. The adapter must capture the header from the initialize
    /// response BEFORE consuming the body and echo it back on the
    /// `notifications/initialized` POST and on the follow-up `send_request`.
    #[tokio::test]
    async fn test_session_id_captured_and_replayed_on_subsequent_requests() {
        // require_session_id = true → initialize sets Mcp-Session-Id and any
        // follow-up POST without the header gets a 400. GET listener returns
        // 405 so the test doesn't depend on the SSE channel.
        let (url, server) = start_fake_http_server_with_options(true, true).await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect("initialize succeeds and captures Mcp-Session-Id");

        // Field populated from the response header.
        assert_eq!(
            *adapter.session_id.read().await,
            Some(FAKE_SESSION_ID.to_string()),
            "session_id should be captured from initialize response header"
        );

        // Follow-up POST works only because the adapter replays the header.
        let result = adapter
            .send_request("tools/call", Some(json!({"name": "x"})))
            .await
            .expect("send_request must replay Mcp-Session-Id on subsequent POSTs");
        assert_eq!(result["ok"], true);

        server.abort();
    }

    /// Backward-compat: server does NOT send `Mcp-Session-Id` on initialize
    /// (e.g. the in-tree fake or any non-session upstream). The adapter must
    /// leave its session_id slot `None` and continue to function without
    /// adding the header on subsequent requests.
    #[tokio::test]
    async fn test_initialize_without_session_id_header_is_backward_compatible() {
        // require_session_id = false → initialize handler omits the header.
        let (url, server) = start_fake_http_server_with_options(true, false).await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect("initialize succeeds even without Mcp-Session-Id");

        assert!(
            adapter.session_id.read().await.is_none(),
            "session_id should remain None when upstream omits the header"
        );

        let result = adapter
            .send_request("tools/call", Some(json!({"name": "x"})))
            .await
            .expect("send_request succeeds without a session header");
        assert_eq!(result["ok"], true);

        server.abort();
    }

    // --- JIT 401 interception (Wave 2 Path B) ---

    /// Fixture whose `POST /mcp` gates every call with a hard 401 + Bearer
    /// `WWW-Authenticate`, and which also advertises full standard OAuth so the
    /// attached [`JitInterceptor`] can self-initiate the flow.
    async fn spawn_gated_mcp_fixture() -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::http::{header::WWW_AUTHENTICATE, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};

        async fn mcp(State(base): State<String>) -> impl IntoResponse {
            let val = format!(
                "Bearer realm=\"Test\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                base
            );
            (
                StatusCode::UNAUTHORIZED,
                [(WWW_AUTHENTICATE, val)],
                "unauthorized",
            )
        }
        async fn protected_resource(State(base): State<String>) -> Json<serde_json::Value> {
            Json(json!({ "resource": base, "authorization_servers": [base] }))
        }
        async fn auth_server(State(base): State<String>) -> Json<serde_json::Value> {
            Json(json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                "token_endpoint": format!("{}/token", base),
                "registration_endpoint": format!("{}/register", base),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<serde_json::Value> {
            Json(json!({ "client_id": "jit-cid", "client_secret": "jit-secret" }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let router = Router::new()
            .route("/mcp", post(mcp))
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource),
            )
            .route("/.well-known/oauth-authorization-server", get(auth_server))
            .route("/register", post(register))
            .with_state(base.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (base, handle)
    }

    /// Fixture whose `POST /mcp` returns `200 {ok:true}` ONLY when the request
    /// carries `Authorization: Bearer <expected>`; otherwise it returns a hard
    /// 401 + Bearer `WWW-Authenticate`. Models the authenticated-retry upstream:
    /// after the human signs in, the persisted bearer is injected and the call
    /// succeeds.
    async fn spawn_bearer_gated_mcp_fixture(
        expected_token: &str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::http::{header::AUTHORIZATION, header::WWW_AUTHENTICATE, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};

        #[derive(Clone)]
        struct FixtureState {
            base: String,
            expected: String,
        }

        async fn mcp(
            State(st): State<FixtureState>,
            req: axum::extract::Request,
        ) -> axum::response::Response {
            let authed = req
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v == format!("Bearer {}", st.expected))
                .unwrap_or(false);
            if authed {
                // The application/json response path does not validate the
                // JSON-RPC id, so a fixed-id success result is sufficient.
                Json(json!({"jsonrpc": "2.0", "result": {"ok": true}, "id": 1})).into_response()
            } else {
                let val = format!(
                    "Bearer realm=\"Test\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                    st.base
                );
                (
                    StatusCode::UNAUTHORIZED,
                    [(WWW_AUTHENTICATE, val)],
                    "unauthorized",
                )
                    .into_response()
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let st = FixtureState {
            base: base.clone(),
            expected: expected_token.to_string(),
        };
        let router = Router::new().route("/mcp", post(mcp)).with_state(st);
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (base, handle)
    }

    /// With a JIT interceptor attached, a gated tool call's 401 is SWALLOWED
    /// (never forwarded as a raw `HttpError { 401 }`) and the produced authorize
    /// URL is SURFACED to the downstream client as an actionable tool result
    /// (`isError: true` with an "open this to sign in" instruction). The raw
    /// upstream challenge is never leaked.
    #[tokio::test]
    async fn call_tool_401_with_interceptor_surfaces_authorize_url() {
        use crate::adapter::oauth::jit::JitInterceptor;
        use crate::oauth::OAuthFlowManager;

        let (base, server) = spawn_gated_mcp_fixture().await;
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let interceptor = Arc::new(JitInterceptor::new(9400, flow_mgr, None, true));

        let mut adapter = HttpAdapter::new(HttpConfig::new(format!("{}/mcp", base)));
        adapter.set_jit_interceptor(interceptor.clone());

        let result = adapter.call_tool("search", json!({})).await;
        let value = match result {
            Ok(v) => v,
            other => panic!("expected surfaced Ok result, got {:?}", other),
        };

        // Surfaced as a tool-error result, not a forwarded protocol failure.
        assert_eq!(value["isError"], true);
        let text = value["content"][0]["text"]
            .as_str()
            .expect("surfaced content text");
        // Carries the composed authorize URL and a sign-in instruction.
        assert!(
            text.contains(&format!("{}/authorize?", base)),
            "surfaced text should contain the authorize URL, got: {}",
            text
        );
        assert!(text.to_lowercase().contains("sign-in"));
        // The raw upstream 401 / WWW-Authenticate challenge is never leaked.
        assert!(!text.contains("401"));
        assert!(!text.contains("WWW-Authenticate"));

        // State machine advanced and the URL is also stored on the interceptor.
        assert_eq!(
            interceptor.state().await,
            crate::adapter::oauth::OAuthState::NeedsLogin
        );
        assert!(interceptor.pending_authorize_url().await.is_some());

        server.abort();
    }

    /// Once a valid bearer token has been persisted for the endpoint (the state
    /// after the loopback `/oauth/callback` completes the code→token exchange),
    /// the tool-call path injects it and a re-issued call SUCCEEDS — without
    /// re-triggering the JIT flow. This exercises the retry-after-sign-in seam.
    #[tokio::test]
    async fn call_tool_uses_persisted_bearer_and_does_not_retrigger_jit() {
        use crate::adapter::oauth::jit::JitInterceptor;
        use crate::oauth::OAuthFlowManager;
        use crate::token_manager::{TokenManager, TokenSet};

        let token = "good-access-token";
        let (base, server) = spawn_bearer_gated_mcp_fixture(token).await;

        // Persist a valid (unexpired) token under the endpoint name, exactly as
        // the `/oauth/callback` handler does after the human signs in.
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        tm.save(
            "ep-retry",
            &TokenSet {
                access_token: token.to_string(),
                refresh_token: None,
                expires_at: Some(now + 3600),
                token_type: "Bearer".to_string(),
                scope: None,
                issued_at: Some(now),
            },
        )
        .await
        .unwrap();

        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let interceptor = Arc::new(JitInterceptor::new(9400, flow_mgr, Some(tm), true));

        let mut config = HttpConfig::new(format!("{}/mcp", base));
        config.endpoint_name = "ep-retry".to_string();
        let mut adapter = HttpAdapter::new(config);
        adapter.set_jit_interceptor(interceptor.clone());

        let result = adapter.call_tool("search", json!({})).await;
        match result {
            Ok(v) => assert_eq!(v["ok"], true),
            other => panic!("expected authenticated success, got {:?}", other),
        }

        // The JIT flow was never triggered: no NeedsLogin transition, no URL.
        assert_ne!(
            interceptor.state().await,
            crate::adapter::oauth::OAuthState::NeedsLogin
        );
        assert!(interceptor.pending_authorize_url().await.is_none());

        server.abort();
    }

    /// Without an interceptor (the default for every adapter today), a 401 is
    /// forwarded unchanged — confirming the wiring is dormant by default.
    #[tokio::test]
    async fn call_tool_401_without_interceptor_is_forwarded_unchanged() {
        let (base, server) = spawn_gated_mcp_fixture().await;
        let adapter = HttpAdapter::new(HttpConfig::new(format!("{}/mcp", base)));
        let result = adapter.call_tool("search", json!({})).await;
        match result {
            Err(AdapterError::HttpError { status: 401, .. }) => {}
            other => panic!("expected HttpError {{ 401 }}, got {:?}", other),
        }
        server.abort();
    }

    // --- 2026 stateless Streamable HTTP path (T8) ---

    #[test]
    fn test_inject_client_info_creates_and_preserves_params() {
        // None params → a fresh object carrying only `_meta` clientInfo.
        let injected = HttpAdapter::inject_client_info(None).unwrap();
        let ci = &injected["_meta"][protocol::META_CLIENT_INFO_KEY];
        assert_eq!(ci["name"], "endara-relay");
        assert!(ci["version"].is_string());

        // Existing fields are preserved; `_meta` clientInfo is added.
        let injected =
            HttpAdapter::inject_client_info(Some(json!({"name": "echo", "arguments": {}})))
                .unwrap();
        assert_eq!(injected["name"], "echo");
        assert_eq!(
            injected["_meta"][protocol::META_CLIENT_INFO_KEY]["name"],
            "endara-relay"
        );
    }

    /// Per-POST capture for the 2026 fixture below.
    struct Captured2026 {
        method: String,
        protocol_version: Option<String>,
        mcp_method: Option<String>,
        mcp_name: Option<String>,
        had_session_id: bool,
        meta_client_info: Option<Value>,
    }

    #[derive(Clone)]
    struct Server2026State {
        seen: Arc<Mutex<Vec<Captured2026>>>,
    }

    async fn dispatch_2026(
        State(app): State<Server2026State>,
        req: axum::extract::Request,
    ) -> axum::response::Response {
        if req.method() != axum::http::Method::POST {
            // 2026 fixture does not implement the GET server-initiated stream.
            return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
        }
        let headers = req.headers().clone();
        let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => return (StatusCode::BAD_REQUEST, "bad body").into_response(),
        };
        let body: Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => return (StatusCode::BAD_REQUEST, "bad json").into_response(),
        };
        let method = body["method"].as_str().unwrap_or("").to_string();
        let id = body["id"].as_u64();
        let header_str = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        app.seen.lock().await.push(Captured2026 {
            method: method.clone(),
            protocol_version: header_str("mcp-protocol-version"),
            mcp_method: header_str("mcp-method"),
            mcp_name: header_str("mcp-name"),
            had_session_id: headers.get("mcp-session-id").is_some(),
            meta_client_info: body
                .get("params")
                .and_then(|p| p.get("_meta"))
                .and_then(|m| m.get(protocol::META_CLIENT_INFO_KEY))
                .cloned(),
        });

        let result = match method.as_str() {
            "server/discover" => json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "stateless-2026", "version": "1.0.0"},
                "tools": []
            }),
            "tools/list" => json!({
                "tools": [{
                    "name": "echo",
                    "description": "Echoes",
                    "inputSchema": {"type": "object"}
                }]
            }),
            "tools/call" => json!({"content": [{"type": "text", "text": "ok"}]}),
            _ => {
                return Json(json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32601, "message": "Method not found"},
                    "id": id,
                }))
                .into_response();
            }
        };
        // 2026 is stateless: the server never emits `Mcp-Session-Id`.
        if id.is_some() {
            Json(json!({"jsonrpc": "2.0", "result": result, "id": id})).into_response()
        } else {
            (StatusCode::ACCEPTED, "").into_response()
        }
    }

    async fn start_fake_2026_http_server() -> (
        String,
        Arc<Mutex<Vec<Captured2026>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = Server2026State { seen: seen.clone() };
        let app = Router::new()
            .route("/mcp", any(dispatch_2026))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/mcp", addr);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, seen, handle)
    }

    /// 2026 upstream: the `server/discover` probe detects `2026-07-28`, so the
    /// adapter skips `initialize`/`notifications/initialized`, captures no
    /// session id, and every POST carries the 2026 routing headers
    /// (`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` for tool calls) plus
    /// `_meta` clientInfo and never an `Mcp-Session-Id`.
    #[tokio::test]
    async fn test_2026_upstream_stateless_path_headers_and_no_handshake() {
        let (url, seen, server) = start_fake_2026_http_server().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect("2026 initialize succeeds");

        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(
            adapter.upstream_dialect().await.is_2026(),
            "upstream should be detected as 2026"
        );
        assert!(
            adapter.session_id.read().await.is_none(),
            "no session id is captured on the 2026 stateless path"
        );

        let tools = adapter.list_tools().await.expect("list_tools");
        assert_eq!(tools.len(), 1);
        let call = adapter
            .call_tool("echo", json!({"message": "hi"}))
            .await
            .expect("call_tool");
        assert_eq!(call["content"][0]["text"], "ok");

        let seen = seen.lock().await;
        let methods: Vec<&str> = seen.iter().map(|c| c.method.as_str()).collect();
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

        for c in seen.iter() {
            assert_eq!(
                c.protocol_version.as_deref(),
                Some("2026-07-28"),
                "every 2026 POST carries MCP-Protocol-Version ({})",
                c.method
            );
            assert_eq!(
                c.mcp_method.as_deref(),
                Some(c.method.as_str()),
                "Mcp-Method mirrors the JSON-RPC method"
            );
            assert!(
                !c.had_session_id,
                "2026 POSTs never carry Mcp-Session-Id ({})",
                c.method
            );
            assert_eq!(
                c.meta_client_info
                    .as_ref()
                    .and_then(|v| v.get("name"))
                    .and_then(|n| n.as_str()),
                Some("endara-relay"),
                "every 2026 POST carries _meta clientInfo ({})",
                c.method
            );
        }

        let call_rec = seen
            .iter()
            .find(|c| c.method == "tools/call")
            .expect("tools/call recorded");
        assert_eq!(
            call_rec.mcp_name.as_deref(),
            Some("echo"),
            "Mcp-Name mirrors the tools/call tool name"
        );
        let list_rec = seen
            .iter()
            .find(|c| c.method == "tools/list")
            .expect("tools/list recorded");
        assert!(
            list_rec.mcp_name.is_none(),
            "Mcp-Name is absent for methods without a tool name"
        );

        drop(seen);
        server.abort();
    }
}
