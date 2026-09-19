use super::oauth::jit::{self, JitInterceptor};
use super::server_name::{sanitize_server_name, ServerNameError};
use super::server_type_resolution::{effective_server_type, strip_mcp_server_suffix};
use super::sse::CrashTracker;
use super::stdio::{iso8601_now, RingBuffer};
use super::{
    connect_error_message, format_error_chain, AdapterError, HealthStatus, McpAdapter, ToolInfo,
    DISCOVER_PROBE_TIMEOUT,
};
use crate::events::{
    annotations_from_value, current_request_context, ToolCallEvent, ToolCallEventBus,
};
use crate::jsonrpc::{self, JsonRpcResponse};
use crate::local_network::with_local_network_hint;
use crate::protocol::{self, detect_upstream_dialect, ProtocolVersion};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};
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
///
/// All mutable state is `Arc`-shared so a [`HttpAdapter::task_clone`] can be
/// handed to the background reconnect supervisor task.
pub struct HttpAdapter {
    config: HttpConfig,
    client: Client,
    health: Arc<RwLock<HealthStatus>>,
    request_id: Arc<AtomicU64>,
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
    /// Once-guard for the span's `server_type` field. `Span::record` appends
    /// each write to the span's field list, so recording on every
    /// [`Self::initialize`] call (e.g. across reconnects) grows the
    /// `endpoint{…}` header without bound. This flag is flipped the first
    /// time a non-empty `server_type` is written so subsequent handshakes
    /// skip the record call.
    server_type_recorded: Arc<AtomicBool>,
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
    ///
    /// This and [`Self::reconnect_handle`] are `std` mutexes (see
    /// [`lock_slot`]): [`Drop`] is synchronous and must drain the slots
    /// unconditionally, even while a concurrent
    /// [`Self::spawn_get_listener`] is mid-installation. No holder ever
    /// awaits while holding one, so the wait is bounded by a spawn/abort.
    listener_handle: Arc<StdMutex<Option<JoinHandle<()>>>>,
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
    /// Upstream `ttlMs` freshness hint (SEP-2549) captured from the most recent
    /// successful `tools/list` result. `Some(ms)` only for 2026 upstreams that
    /// sent a top-level `ttlMs`; `None` otherwise. Read by the registry cache to
    /// honor the upstream's freshness window. See [`Self::list_tools_ttl_ms`].
    list_ttl_ms: Arc<RwLock<Option<u64>>>,
    /// Count of consecutive transport-level ("upstream is dead") failures seen
    /// on outbound requests issued via [`Self::send_request`] (the `list_tools`
    /// and `call_tool` paths). Incremented only for transport-dead signals
    /// (see [`Self::is_transport_dead`]); reset to 0 on any successful request.
    /// Once it reaches [`TRANSPORT_FAILURE_THRESHOLD`] the adapter flips its
    /// own health to `Unhealthy("upstream unreachable")`, giving plain HTTP the
    /// post-init death detection it otherwise lacks (no background heartbeat).
    /// Recovery happens either reactively (next successful request, see
    /// [`Self::note_request_success`]) or proactively via the reconnect
    /// supervisor (see [`Self::run_supervisor`]).
    transport_failures: Arc<AtomicU64>,
    /// Latched `true` by [`Self::mark_handshake_healthy`] the first time a
    /// handshake (caller-owned or supervisor) commits. While it is still
    /// `false` and health is `Unhealthy`, the adapter has never had a
    /// session: [`Self::list_tools`] answers `Ok([])` without network I/O
    /// (matching a `FailedAdapter`, so a dead upstream adds no connect
    /// latency or error traffic to every catalog rebuild) and
    /// [`Self::note_request_success`] never flips health to `Healthy` — only
    /// a real handshake may do that. Cleared again by `shutdown()`, which
    /// tears the session listener down: until the next handshake respawns
    /// it, a stray success on the old session must not promote the adapter
    /// (the endpoint would miss server-initiated tool changes). Otherwise
    /// the pre-existing reactive recovery semantics apply unchanged.
    handshake_completed: Arc<AtomicBool>,
    /// Exponential-backoff state for the reconnect supervisor (1 s base, ×2,
    /// 60 s cap — same escalation as the SSE adapter). Reset on every
    /// successful handshake.
    crash_tracker: Arc<Mutex<CrashTracker>>,
    /// Handle for the background reconnect supervisor task spawned by
    /// [`Self::ensure_supervisor_running`]. Aborted on shutdown / drop.
    reconnect_handle: Arc<StdMutex<Option<JoinHandle<()>>>>,
    /// Notified (`notify_waiters`) on every actual `Unhealthy → Healthy` flip
    /// so an in-flight supervisor attempt that has not yet sent `initialize`
    /// can stand down instead of running a redundant handshake.
    recovered_notify: Arc<Notify>,
    /// Incremented (under the `health` write lock) on every actual
    /// `Unhealthy → Healthy` flip, reactive or handshake. Lets a wrapper that
    /// caches its own verdict (the OAuth heartbeat) tell a genuine recovery
    /// apart from an ordinary `tools/list_changed` tick on the same
    /// broadcast: both arrive as a tick, only the former advances this
    /// counter. See [`Self::recovery_generation`].
    recovery_generation: Arc<AtomicU64>,
    /// Notified by [`Self::note_transport_failure`] when reactive health flips
    /// the adapter to `Unhealthy`, waking the supervisor to start reconnecting.
    reconnect_notify: Arc<Notify>,
    /// Serializes handshakes. The caller-owned [`McpAdapter::initialize`]
    /// (`&mut self`) and the supervisor's [`Self::task_clone`] share the
    /// session and health state, so `initialize()` can overlap an in-flight
    /// reconnect handshake — the management enable path re-initializes an
    /// already-enabled endpoint while its supervisor may be mid-attempt. Two
    /// concurrent `initialize` exchanges whose responses complete out of
    /// order would leave the OLDER session id in [`Self::session_id`] while
    /// the upstream honours the newer one. Both paths hold this across
    /// the dialect probe AND [`Self::complete_handshake`], so the committed
    /// session is always the one from the last-completed handshake and a
    /// probe result is never committed after another handshake changed the
    /// state it was taken against. An aborted supervisor (shutdown) drops
    /// its guard with its future, so `shutdown()` → `initialize()` never
    /// waits on a dead holder.
    handshake_lock: Arc<Mutex<()>>,
    /// Set by `shutdown()` / [`Drop`] before the background tasks are torn
    /// down. [`Self::spawn_get_listener`] checks it under the listener slot
    /// lock so a handshake racing shutdown never installs a fresh listener
    /// that nobody will abort.
    shutting_down: Arc<AtomicBool>,
    /// Test-only barrier at the last cancellation point of a handshake: after
    /// the JIT credential lookup, before the legacy `initialize` POST, or
    /// right before the 2026 commit ([`Self::commit_2026_handshake`]). Lets
    /// a test order a reactive recovery deterministically into that window.
    #[cfg(test)]
    handshake_send_gate: Option<Arc<TestGate>>,
    /// `true` on the adapter the caller owns, `false` on the
    /// [`Self::task_clone`] handed to the supervisor task. Only the owning
    /// instance tears down background tasks in [`Drop`]; otherwise the
    /// supervisor's own clone would abort the GET listener (and itself) the
    /// moment it exited.
    owns_background_tasks: bool,
}

/// Outcome of [`HttpAdapter::complete_handshake`].
#[derive(Debug, PartialEq, Eq)]
enum HandshakeOutcome {
    /// The handshake ran to completion and committed `Healthy`.
    Completed,
    /// A supervisor attempt found the adapter already recovered at its last
    /// cancellation point and sent nothing; the shared state is untouched.
    Obsolete,
}

/// Two-sided test barrier (see [`HttpAdapter::handshake_send_gate`]): the
/// adapter signals `reached` and parks on `release`.
#[cfg(test)]
pub(crate) struct TestGate {
    pub(crate) reached: Notify,
    pub(crate) release: Notify,
}

#[cfg(test)]
impl TestGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            reached: Notify::new(),
            release: Notify::new(),
        })
    }
}

/// A validated upstream identity, extracted from an `initialize` or
/// `server/discover` result by [`HttpAdapter::validate_server_identity`] and
/// written to the shared state by [`HttpAdapter::commit_server_identity`].
struct ServerIdentity {
    effective: Option<String>,
    upstream_stripped: String,
}

/// Lock a background-task handle slot ([`HttpAdapter::listener_handle`] /
/// [`HttpAdapter::reconnect_handle`]). A poisoned slot only ever holds a
/// join handle, so it is still safe to use.
fn lock_slot(
    slot: &StdMutex<Option<JoinHandle<()>>>,
) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Consecutive transport-dead failures on `send_request` before the plain HTTP
/// adapter flips its health to `Unhealthy("upstream unreachable")`. A single
/// blip shouldn't demote a server, but a server that's actually gone fails
/// every request, so a small threshold both avoids flapping and reports death
/// quickly.
const TRANSPORT_FAILURE_THRESHOLD: u64 = 3;

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
        Self::with_span(config, client, span)
    }

    /// Create a new HttpAdapter with a pre-built reqwest::Client.
    ///
    /// Top-level constructor kept for completeness; OAuth wrapping uses
    /// [`HttpAdapter::new_with_client_inner`] instead so the inner adapter
    /// shares the wrapper's `endpoint` tracing span rather than creating
    /// its own.
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
    /// `endpoint` tracing span. The wrapper passes that span in so the inner
    /// adapter's `.instrument(self.span.clone())` calls enter the wrapper's
    /// span and its events (tool-call completed/failed lines, handshake
    /// logging) carry the `endpoint`/`transport` fields the desktop Logs tab
    /// attributes lines by. The inner adapter never builds a span of its own,
    /// so exactly one `endpoint` span is in scope — no duplicated
    /// `endpoint=<name>` field.
    ///
    /// The `server_type` once-guard is pre-flipped: the wrapper owns that
    /// record on the shared span (behind its own once-guard), and inner
    /// adapters are rebuilt on every token swap — a fresh inner guard would
    /// re-append `server_type` to the shared span on each rebuild.
    pub fn new_with_client_inner(config: HttpConfig, client: Client, span: tracing::Span) -> Self {
        let adapter = Self::with_span(config, client, span);
        adapter.server_type_recorded.store(true, Ordering::Relaxed);
        adapter
    }

    fn with_span(config: HttpConfig, client: Client, span: tracing::Span) -> Self {
        let (tools_changed_tx, _) = broadcast::channel(16);
        Self {
            config,
            client,
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: Arc::new(AtomicU64::new(1)),
            server_type: Arc::new(RwLock::new(None)),
            upstream_server_name: Arc::new(RwLock::new(None)),
            activity_log: Arc::new(RwLock::new(RingBuffer::new(1000))),
            span,
            server_type_recorded: Arc::new(AtomicBool::new(false)),
            tools_changed_tx,
            listener_handle: Arc::new(StdMutex::new(None)),
            shutdown_notify: Arc::new(Notify::new()),
            event_bus: Arc::new(OnceLock::new()),
            tool_annotations_cache: Arc::new(RwLock::new(HashMap::new())),
            session_id: Arc::new(RwLock::new(None)),
            jit_interceptor: None,
            last_www_authenticate: Arc::new(RwLock::new(None)),
            upstream_dialect: Arc::new(RwLock::new(ProtocolVersion::V2025_03_26)),
            list_ttl_ms: Arc::new(RwLock::new(None)),
            transport_failures: Arc::new(AtomicU64::new(0)),
            handshake_completed: Arc::new(AtomicBool::new(false)),
            crash_tracker: Arc::new(Mutex::new(CrashTracker::new())),
            reconnect_handle: Arc::new(StdMutex::new(None)),
            recovered_notify: Arc::new(Notify::new()),
            recovery_generation: Arc::new(AtomicU64::new(0)),
            reconnect_notify: Arc::new(Notify::new()),
            handshake_lock: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            handshake_send_gate: None,
            owns_background_tasks: true,
        }
    }

    /// Build a non-owning handle onto the same shared state for the
    /// background reconnect supervisor. Every field is `Arc`-shared (or cheaply
    /// `Clone`), so the supervisor observes and mutates the same health /
    /// session / counters as the caller-owned adapter. The clone's [`Drop`] is
    /// a no-op (`owns_background_tasks == false`).
    fn task_clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            health: self.health.clone(),
            request_id: self.request_id.clone(),
            server_type: self.server_type.clone(),
            upstream_server_name: self.upstream_server_name.clone(),
            activity_log: self.activity_log.clone(),
            span: self.span.clone(),
            server_type_recorded: self.server_type_recorded.clone(),
            tools_changed_tx: self.tools_changed_tx.clone(),
            listener_handle: self.listener_handle.clone(),
            shutdown_notify: self.shutdown_notify.clone(),
            event_bus: self.event_bus.clone(),
            tool_annotations_cache: self.tool_annotations_cache.clone(),
            session_id: self.session_id.clone(),
            jit_interceptor: self.jit_interceptor.clone(),
            last_www_authenticate: self.last_www_authenticate.clone(),
            upstream_dialect: self.upstream_dialect.clone(),
            list_ttl_ms: self.list_ttl_ms.clone(),
            transport_failures: self.transport_failures.clone(),
            handshake_completed: self.handshake_completed.clone(),
            crash_tracker: self.crash_tracker.clone(),
            reconnect_handle: self.reconnect_handle.clone(),
            recovered_notify: self.recovered_notify.clone(),
            recovery_generation: self.recovery_generation.clone(),
            reconnect_notify: self.reconnect_notify.clone(),
            handshake_lock: self.handshake_lock.clone(),
            shutting_down: self.shutting_down.clone(),
            #[cfg(test)]
            handshake_send_gate: self.handshake_send_gate.clone(),
            owns_background_tasks: false,
        }
    }

    /// Record `server_type` on the per-endpoint span at most once. `Span::record`
    /// appends each write to the span's field list, so recording on every
    /// [`Self::initialize`] call (e.g. across HTTP/SSE reconnects) grows the
    /// `endpoint{…}` header without bound. The guard flips the first time a
    /// non-empty name is written so subsequent handshakes are a no-op.
    fn record_server_type_once(&self, name: &str) {
        if !self.server_type_recorded.swap(true, Ordering::Relaxed) {
            self.span
                .record("server_type", tracing::field::display(name));
        }
    }

    /// Test-only accessor for the `server_type` once-guard state.
    #[cfg(test)]
    pub(crate) fn server_type_recorded_flag(&self) -> bool {
        self.server_type_recorded.load(Ordering::Relaxed)
    }

    /// Test-only: cross the transport-failure threshold exactly as a run of
    /// dead requests would, demoting the adapter and arming its supervisor.
    /// Lets wrapper tests (OAuth) drive a real `Unhealthy → Healthy` flip
    /// through the production recovery paths.
    #[cfg(test)]
    pub(crate) async fn demote_via_transport_failures_for_test(&self) {
        for _ in 0..TRANSPORT_FAILURE_THRESHOLD {
            self.note_transport_failure().await;
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

    /// The persisted JIT bearer for `endpoint_name` as an `Authorization`
    /// header value, or `None` when no interceptor is attached, no token is
    /// stored, or the stored token has expired
    /// ([`JitInterceptor::current_bearer`]). Read live on every request so a
    /// token refreshed after the loopback `/oauth/callback` is picked up
    /// without rebuilding the adapter. The value is marked sensitive so
    /// request/header debug output redacts it; the token is never logged.
    ///
    /// Static so the GET listener task (which owns no `&self`) can use the
    /// same code path as the adapter's own request sites.
    async fn jit_bearer_header(
        interceptor: Option<&Arc<JitInterceptor>>,
        endpoint_name: &str,
    ) -> Option<reqwest::header::HeaderValue> {
        let token = interceptor?.current_bearer(endpoint_name).await?;
        let mut val = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token)).ok()?;
        val.set_sensitive(true);
        Some(val)
    }

    /// Single seam through which every request this adapter puts on the
    /// wire — tool calls, `server/discover`, `initialize`,
    /// `notifications/initialized` and other notifications — receives the
    /// current JIT bearer (see [`Self::jit_bearer_header`]). No-op without an
    /// interceptor or a valid stored token, leaving the default request path
    /// unchanged. The GET listener applies the same header per request via
    /// [`Self::jit_bearer_header`] directly.
    async fn apply_jit_bearer(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match Self::jit_bearer_header(self.jit_interceptor.as_ref(), &self.config.endpoint_name)
            .await
        {
            Some(val) => builder.header(reqwest::header::AUTHORIZATION, val),
            None => builder,
        }
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

    /// Number of `Unhealthy → Healthy` flips so far. A wrapper snapshots it
    /// and treats a later, larger value as proof that a genuine recovery
    /// happened in between (an ordinary `tools/list_changed` tick leaves it
    /// unchanged). Bumped before the flip's tick is sent, so a subscriber
    /// that reads it on receipt of the tick sees the new value.
    pub(crate) fn recovery_generation(&self) -> u64 {
        self.recovery_generation.load(Ordering::SeqCst)
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

    /// Apply the 2026 Streamable HTTP per-request headers to `builder`:
    /// `MCP-Protocol-Version` (always), `Mcp-Method` (the JSON-RPC method), and
    /// `Mcp-Name` (the `params.name` tool name, for `tools/call` only). These let
    /// a 2026 upstream route/observe a request without parsing its body.
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
        // `Mcp-Name` is tied to `tools/call` by the 2026-07-28 spec: only emit it
        // for that method, even if another method happens to carry a string
        // `params.name`.
        if method == "tools/call" {
            if let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) {
                if let Ok(val) = reqwest::header::HeaderValue::from_str(name) {
                    builder = builder.header(MCP_NAME_HEADER_NAME.clone(), val);
                }
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
        let builder = self.apply_jit_bearer(builder).await;
        let resp = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                AdapterError::Timeout(self.config.timeout_secs)
            } else if e.is_connect() {
                AdapterError::ConnectionFailed(with_local_network_hint(
                    &self.config.url,
                    connect_error_message(&self.config.url, &e),
                ))
            } else {
                AdapterError::HttpError {
                    status: 0,
                    body: format_error_chain(&e),
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

    /// Classify an [`AdapterError`] as a transport-dead ("upstream is gone")
    /// signal versus an alive-but-erroring one. Only the former feed the
    /// consecutive-failure counter that drives reactive health.
    ///
    /// - Dead: [`AdapterError::ConnectionFailed`] (reqwest `is_connect`),
    ///   [`AdapterError::Timeout`] (`is_timeout`), and
    ///   [`AdapterError::HttpError`] with `status == 0` (other reqwest send
    ///   errors) — the server didn't answer at the transport level.
    /// - Not dead: any `HttpError` with a non-zero status (401/404/500…),
    ///   `JsonRpcError`, and `ProtocolError` — the server is up and responding,
    ///   so these must never demote health (a 401 means the server is alive).
    fn is_transport_dead(err: &AdapterError) -> bool {
        matches!(
            err,
            AdapterError::ConnectionFailed(_)
                | AdapterError::Timeout(_)
                | AdapterError::HttpError { status: 0, .. }
        )
    }

    /// Record a successful request: reset the transport-failure counter and, if
    /// the adapter had previously demoted itself to `Unhealthy`, recover to
    /// `Healthy`. Other health states (`Starting`/`Stopped`) are left untouched.
    ///
    /// The counter reset and the health write are performed while holding the
    /// `health` write lock so they transition atomically with respect to
    /// [`Self::note_transport_failure`] — there is no interleaving that can
    /// leave the counter and health disagreeing.
    ///
    /// On an actual `Unhealthy` → `Healthy` flip (only — not on every
    /// success) a `tools_changed` tick is emitted so the registry invalidates
    /// any merged catalog rebuilt without this endpoint's tools during the
    /// outage. The tick is sent AFTER the statement-scoped write guard drops
    /// (mirroring the SSE adapter's post-reconnect tick): [`Self::health`]
    /// reads via `try_read` with a `Starting` fallback, so a catalog rebuild
    /// racing a tick sent under the guard would mislabel the endpoint
    /// UNAVAILABLE with no corrective tick to follow. A failure interleaving
    /// between the drop and the send is benign — it just re-demotes health
    /// and the next recovery flip ticks again. `SendError` (no subscribers)
    /// is harmless — drop it.
    ///
    /// The flip also fires [`Self::recovered_notify`] so a reconnect attempt
    /// the supervisor has in flight (but that has not yet sent `initialize`)
    /// stands down instead of handshaking redundantly.
    ///
    /// A never-initialized adapter (see [`Self::handshake_completed`]) is not
    /// recovered here: a stray successful request proves the upstream is up,
    /// but the adapter still has no session, so only a handshake may flip it.
    async fn note_request_success(&self) {
        let flipped = {
            let mut health = self.health.write().await;
            self.transport_failures.store(0, Ordering::SeqCst);
            if self.handshake_completed.load(Ordering::SeqCst)
                && matches!(*health, HealthStatus::Unhealthy(_))
            {
                *health = HealthStatus::Healthy;
                self.recovery_generation.fetch_add(1, Ordering::SeqCst);
                true
            } else {
                false
            }
        };
        if flipped {
            self.recovered_notify.notify_waiters();
            let _ = self.tools_changed_tx.send(());
        }
    }

    /// Record a transport-dead failure: increment the consecutive-failure
    /// counter and, once it reaches [`TRANSPORT_FAILURE_THRESHOLD`], flip health
    /// to `Unhealthy("upstream unreachable")`.
    ///
    /// The increment, threshold check, and health write are performed while
    /// holding the `health` write lock so they transition atomically with
    /// respect to [`Self::note_request_success`]. This guarantees there is no
    /// interleaving that leaves `health == Unhealthy("upstream unreachable")`
    /// while the failure counter is below the threshold.
    ///
    /// On the actual demotion (a non-`Unhealthy`, non-`Stopped` state crossing
    /// the threshold) the reconnect supervisor is woken so recovery no longer
    /// depends on a caller happening to issue another request. Repeated
    /// failures while already `Unhealthy` only refresh the reason; the
    /// supervisor is already retrying. A `Stopped` adapter never spawns one.
    async fn note_transport_failure(&self) {
        let demoted = {
            let mut health = self.health.write().await;
            let count = self.transport_failures.fetch_add(1, Ordering::SeqCst) + 1;
            if count >= TRANSPORT_FAILURE_THRESHOLD {
                let was_live =
                    !matches!(*health, HealthStatus::Unhealthy(_) | HealthStatus::Stopped);
                *health = HealthStatus::Unhealthy("upstream unreachable".into());
                was_live
            } else {
                false
            }
        };
        if demoted {
            self.ensure_supervisor_running().await;
            self.reconnect_notify.notify_one();
        }
    }

    /// Send a JSON-RPC request via HTTP POST and return the result.
    ///
    /// Wraps [`Self::send_request_inner`] to centralize reactive health: the
    /// outcome of every `list_tools` / `call_tool` request feeds the
    /// transport-failure counter so a plain HTTP server that dies after init is
    /// detected (flips to `Unhealthy`) and auto-recovers on the next success.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let result = self.send_request_inner(method, params).await;
        match &result {
            Ok(_) => self.note_request_success().await,
            Err(e) if Self::is_transport_dead(e) => self.note_transport_failure().await,
            // Alive-but-erroring (HTTP status>0, JSON-RPC, protocol): leave the
            // counter and health untouched so a 401/500/etc. never demotes.
            Err(_) => {}
        }
        result
    }

    /// Inner request implementation: builds and sends the HTTP POST and maps the
    /// response/transport outcome to a `Result`. Health bookkeeping lives in the
    /// [`Self::send_request`] wrapper.
    async fn send_request_inner(
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
        let builder = self.apply_jit_bearer(builder).await;
        let resp = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                AdapterError::Timeout(self.config.timeout_secs)
            } else if e.is_connect() {
                AdapterError::ConnectionFailed(with_local_network_hint(
                    &self.config.url,
                    connect_error_message(&self.config.url, &e),
                ))
            } else {
                AdapterError::HttpError {
                    status: 0,
                    body: format_error_chain(&e),
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
        jit_interceptor: Option<Arc<JitInterceptor>>,
        endpoint_name: String,
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

        // Same JIT bearer seam as the adapter's POST sites, read live at
        // connect time so every (re)spawned stream carries the current token.
        let mut get = client
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(val) = Self::jit_bearer_header(jit_interceptor.as_ref(), &endpoint_name).await {
            get = get.header(reqwest::header::AUTHORIZATION, val);
        }
        let resp = tokio::select! {
            _ = shutdown.notified() => return,
            r = get.send() => match r {
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

        // Bound the probe with a short dedicated timeout (below the full reqwest
        // transport timeout) so a legacy/unresponsive upstream that silently
        // drops the unknown request falls back to the legacy handshake fast. A
        // timeout maps to `None`, the same clean legacy fallback as any other
        // failure.
        let probe = async {
            let builder = self.client.post(&self.config.url).json(&request);
            let builder =
                Self::apply_2026_headers(builder, "server/discover", request.params.as_ref());
            let builder = self.apply_jit_bearer(builder).await;
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
        };

        tokio::time::timeout(DISCOVER_PROBE_TIMEOUT, probe)
            .await
            .ok()
            .flatten()
    }

    /// Extract, validate, and record the upstream `serverInfo.name` from an
    /// `initialize` result: [`Self::validate_server_identity`] followed by
    /// [`Self::commit_server_identity`]. The legacy handshake's post-POST
    /// path, which runs to completion once the upstream has answered.
    async fn apply_server_identity(
        &self,
        result: &Value,
        supervisor_attempt: bool,
    ) -> Result<(), AdapterError> {
        let identity = self
            .validate_server_identity(result, supervisor_attempt)
            .await?;
        self.commit_server_identity(identity).await;
        Ok(())
    }

    /// Extract and validate the upstream `serverInfo.name` from an
    /// `initialize` or `server/discover` result without touching the shared
    /// state. Sets the adapter unhealthy (subject to
    /// [`Self::set_handshake_unhealthy`]) and returns `Err` when the name is
    /// missing or fails sanitization. Shared by the legacy handshake and the
    /// 2026 stateless paths so both name the endpoint identically.
    async fn validate_server_identity(
        &self,
        result: &Value,
        supervisor_attempt: bool,
    ) -> Result<ServerIdentity, AdapterError> {
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
                self.set_handshake_unhealthy(msg.clone(), supervisor_attempt)
                    .await;
                return Err(AdapterError::ProtocolError(msg));
            }
        };

        // Validate and sanitize the server name
        let sanitized = match sanitize_server_name(raw_name) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                error!(url = %self.config.url, raw_name = %raw_name, error = %msg, "serverInfo.name validation failed");
                self.set_handshake_unhealthy(msg.clone(), supervisor_attempt)
                    .await;
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
        Ok(ServerIdentity {
            effective,
            upstream_stripped,
        })
    }

    /// Write a validated identity to the shared state.
    async fn commit_server_identity(&self, identity: ServerIdentity) {
        if let Some(ref name) = identity.effective {
            self.record_server_type_once(name);
        }
        *self.server_type.write().await = identity.effective;
        *self.upstream_server_name.write().await = Some(identity.upstream_stripped);
    }

    /// Spawn the long-lived `GET <url>` SSE listener for server-initiated
    /// notifications (notably `notifications/tools/list_changed`). Snapshots the
    /// current session id at spawn time (always `None` for 2026 stateless
    /// upstreams). Shared by the legacy and 2026 initialize paths. Any
    /// previous listener (from a connection that has since died) is aborted
    /// first so a reconnect never leaves two streams open.
    ///
    /// The listener authenticates from `HttpConfig::headers`, not from
    /// `self.client`: the request client's per-request timeout would tear the
    /// long-lived stream down, so the listener builds its own client and the
    /// OAuth wrapper mirrors its bearer into the config headers. A JIT bearer
    /// is applied per connect via [`Self::jit_bearer_header`], the same seam
    /// as every POST.
    async fn spawn_get_listener(&self) {
        let url = self.config.url.clone();
        let headers = self.config.headers.clone();
        // Snapshot the session ID at spawn time so the listener doesn't
        // need to re-read adapter state. Matches how `headers` is passed.
        let session_id = self.session_id.read().await.clone();
        let jit_interceptor = self.jit_interceptor.clone();
        let endpoint_name = self.config.endpoint_name.clone();
        let tx = self.tools_changed_tx.clone();
        let shutdown = self.shutdown_notify.clone();
        let listener_span = self.span.clone();
        // Take the slot lock BEFORE spawning and store the handle with no
        // await point in between: if the calling task (the reconnect
        // supervisor) is aborted mid-way, it is either before the spawn (no
        // listener exists) or after the handle is stored (shutdown/drop can
        // find and abort it). Spawning first and then awaiting the lock could
        // orphan a live listener. Once shutdown has been signalled, the slot
        // is (or is about to be) drained for good — do not repopulate it:
        // `shutdown()`/`Drop` set the flag BEFORE taking this lock, so either
        // they drain after this section stores the handle, or this section
        // observes the flag and installs nothing.
        let mut slot = lock_slot(&self.listener_handle);
        if self.shutting_down.load(Ordering::SeqCst) {
            debug!(url = %self.config.url, "shutdown in progress; not spawning GET listener");
            return;
        }
        let handle = tokio::spawn(
            async move {
                Self::run_get_listener(
                    url,
                    headers,
                    session_id,
                    jit_interceptor,
                    endpoint_name,
                    tx,
                    shutdown,
                )
                .await;
            }
            .instrument(listener_span),
        );
        if let Some(previous) = slot.replace(handle) {
            previous.abort();
        }
    }

    /// Spawn the background reconnect supervisor task if it isn't running.
    ///
    /// Never installs one once shutdown has begun: `shutdown()`/`Drop` set
    /// `shutting_down` BEFORE they drain this slot, and the check here shares
    /// the slot lock with that drain, so a transport failure (or startup
    /// retry) racing shutdown either stores its handle before the drain
    /// (which then aborts it) or observes the flag and installs nothing. A
    /// supervisor spawned after the drain would have missed the non-latched
    /// `shutdown_notify` and stayed parked forever.
    async fn ensure_supervisor_running(&self) {
        let mut guard = lock_slot(&self.reconnect_handle);
        if self.shutting_down.load(Ordering::SeqCst) {
            debug!(url = %self.config.url, "shutdown in progress; not spawning reconnect supervisor");
            return;
        }
        if guard.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let me = self.task_clone();
        let span = self.span.clone();
        let handle = tokio::spawn(
            async move {
                me.run_supervisor().await;
            }
            .instrument(span),
        );
        *guard = Some(handle);
    }

    /// Arm the reconnect supervisor after a failed [`McpAdapter::initialize`]
    /// so the adapter recovers on its own once the upstream is reachable,
    /// instead of staying frozen in the `Unhealthy(<init error>)` state the
    /// handshake left it in. `initialize()` calls this itself on failure, so
    /// every caller of it — the registration path
    /// ([`crate::watcher::create_adapter`]) at relay startup or on a config
    /// add/reload, and the management enable/restart paths that
    /// re-initialize after a `shutdown()` — gets a plain `http` upstream that
    /// is down retried with the standard backoff, no caller traffic required.
    /// The eventual recovery flips `Unhealthy → Healthy` through
    /// [`Self::mark_handshake_healthy`], which emits the `tools_changed` tick
    /// the registry uses to re-fetch the catalog.
    ///
    /// No-op unless health is `Unhealthy`: a `Stopped` (or already recovered)
    /// adapter must never spawn a supervisor, the same rule
    /// [`Self::note_transport_failure`] follows.
    pub(crate) async fn retry_initialize_in_background(&self) {
        if !matches!(*self.health.read().await, HealthStatus::Unhealthy(_)) {
            return;
        }
        self.ensure_supervisor_running().await;
        self.reconnect_notify.notify_one();
    }

    /// Reconnect supervisor loop — wait for a "went unhealthy" notification
    /// from [`Self::note_transport_failure`], then re-run the handshake
    /// ([`Self::try_discover_probe`] + [`Self::complete_handshake`]) with
    /// exponential backoff until it succeeds. Unlike the SSE supervisor there
    /// is no attempt cap: a plain HTTP upstream that is down for minutes must
    /// still come back on its own once reachable, so the loop keeps retrying
    /// at the 60 s ceiling. Exits only on shutdown.
    ///
    /// Reactive recovery (a caller's request succeeding, see
    /// [`Self::note_request_success`]) can beat the supervisor at any point,
    /// and an attempt it obsoletes must neither handshake redundantly nor
    /// undo the recovery:
    ///
    /// * Before `initialize` is sent (backoff, dialect probe) the attempt is
    ///   cancellable: it is raced against [`Self::recovered_notify`], whose
    ///   `Notified` is created BEFORE the pre-attempt health check so a flip
    ///   landing in between is never missed, and health is re-checked after
    ///   each phase. The signal itself is only a hint: a delayed one from an
    ///   earlier flip must not stand the supervisor down while a later
    ///   demotion has it `Unhealthy` again
    ///   (see [`Self::stood_down_after_recovery_signal`]), and it must not
    ///   restart the attempt either — the pending backoff keeps its original
    ///   deadline and an in-flight dialect probe keeps running, so one
    ///   attempt costs at most one (capped) backoff sleep (pinned by
    ///   `stale_recovery_signal_during_backoff_keeps_the_deadline`).
    /// * Once `initialize` is on the wire the upstream may have rotated the
    ///   session, so the attempt runs to completion: on success the returned
    ///   session replaces the stored one; on failure
    ///   [`Self::set_handshake_unhealthy`] refuses to overwrite a health
    ///   state that is no longer `Unhealthy`. The last health check sits
    ///   directly before the `initialize` POST with no await between them;
    ///   the residual window (a caller's success on another worker) is
    ///   inherent unless the `health` lock were held across network I/O, and
    ///   it is harmless because the attempt commits exactly the session the
    ///   upstream handed it.
    /// * The 2026 stateless path never reaches the upstream, so it has no
    ///   point of no return: its currency check and all of its writes share
    ///   one `health` write lock ([`Self::commit_2026_handshake`]), which
    ///   covers every await before it generically (pinned by
    ///   `recovery_before_2026_commit_keeps_legacy_session`).
    /// * A NEWER demotion that lands while `initialize` is in flight is not
    ///   tracked by a generation: a handshake that then succeeds is the most
    ///   recent evidence and promotes to `Healthy`. Its reconnect permit is
    ///   consumed by a pass that finds the adapter `Healthy` and stands
    ///   down, and since health IS `Healthy` again the next threshold
    ///   crossing demotes and re-arms — no interleaving leaves the adapter
    ///   `Unhealthy` with nothing retrying (pinned by
    ///   `demotion_during_inflight_handshake_promotes_and_keeps_retrying`).
    /// * A caller-owned [`McpAdapter::initialize`] never overlaps the
    ///   attempt: both hold [`Self::handshake_lock`] across
    ///   [`Self::complete_handshake`], so whichever handshake completes last
    ///   is also the last to commit its session (pinned by
    ///   `caller_initialize_waits_for_inflight_supervisor_handshake`).
    ///
    /// The `tools_changed` tick is owned by whichever path performs the
    /// actual `Unhealthy → Healthy` flip (the handshake does it under the
    /// same `health` write lock, see [`Self::mark_handshake_healthy`]), so
    /// exactly one tick is emitted per recovery.
    async fn run_supervisor(&self) {
        loop {
            tokio::select! {
                _ = self.shutdown_notify.notified() => return,
                _ = self.reconnect_notify.notified() => {}
            }

            'attempt: loop {
                // `notify_waiters` only reaches `Notified` futures that
                // already exist, so register before checking health.
                let recovered = self.recovered_notify.notified();
                tokio::pin!(recovered);

                // A stale permit (e.g. a caller's success already recovered
                // the adapter while the notification was pending) or a
                // recovery during the previous attempt makes this one
                // obsolete.
                if !matches!(*self.health.read().await, HealthStatus::Unhealthy(_)) {
                    self.crash_tracker.lock().await.reset();
                    break;
                }

                let backoff = {
                    let mut tracker = self.crash_tracker.lock().await;
                    // The window cap is an SSE concern; HTTP retries forever.
                    let _ = tracker.record_failure();
                    tracker.backoff_duration()
                };

                info!(
                    url = %self.config.url,
                    backoff_ms = backoff.as_millis() as u64,
                    "HTTP reconnect: backing off before next attempt"
                );

                // The deadline is pinned once: a stale recovery hint (see
                // `stood_down_after_recovery_signal`) re-enters the wait for
                // the REMAINING time instead of recording another attempt and
                // starting a fresh — possibly capped — backoff, so one
                // outage costs at most one backoff sleep per attempt.
                let sleep = tokio::time::sleep_until(tokio::time::Instant::now() + backoff);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = self.shutdown_notify.notified() => return,
                        _ = &mut recovered => {
                            // Re-arm BEFORE the health re-read: a genuine flip
                            // landing between the two would otherwise notify
                            // no one and leave this sleep to run out.
                            recovered.set(self.recovered_notify.notified());
                            if self.stood_down_after_recovery_signal("backoff").await {
                                break 'attempt;
                            }
                        }
                        _ = &mut sleep => break,
                    }
                }

                if !matches!(*self.health.read().await, HealthStatus::Unhealthy(_)) {
                    debug!(url = %self.config.url, "HTTP reconnect: recovered reactively during backoff, skipping");
                    self.crash_tracker.lock().await.reset();
                    break;
                }

                info!(url = %self.config.url, "HTTP reconnect: attempting");
                // Serialize with a caller-owned `initialize()` (see
                // `handshake_lock`) BEFORE probing: a probe result taken
                // outside the lock could be committed after the handshake
                // that made this attempt wait already re-demoted the adapter
                // (or the upstream died), and a stateless 2026 result sends
                // nothing fresh in `complete_handshake` to notice. Re-check
                // health once the lock is held: the handshake that made this
                // attempt wait may have recovered the adapter, in which case
                // a second `initialize` would only churn the upstream's
                // session. The probe is bounded by `DISCOVER_PROBE_TIMEOUT`,
                // so a caller waits on the lock for at most that long.
                let handshake = self.handshake_lock.lock().await;
                if !matches!(*self.health.read().await, HealthStatus::Unhealthy(_)) {
                    debug!(url = %self.config.url, "HTTP reconnect: recovered while waiting for the handshake lock, abandoning attempt");
                    self.crash_tracker.lock().await.reset();
                    break;
                }
                // The dialect probe is stateless, so abandoning it mid-flight
                // has no upstream side effect — the last point at which a
                // reactive recovery can retire this attempt. A stale hint
                // leaves the in-flight probe alone.
                let probe = self.try_discover_probe();
                tokio::pin!(probe);
                let discover_result = loop {
                    tokio::select! {
                        _ = self.shutdown_notify.notified() => return,
                        _ = &mut recovered => {
                            recovered.set(self.recovered_notify.notified());
                            if self.stood_down_after_recovery_signal("dialect probe").await {
                                break 'attempt;
                            }
                        }
                        result = &mut probe => break result,
                    }
                };
                if !matches!(*self.health.read().await, HealthStatus::Unhealthy(_)) {
                    debug!(url = %self.config.url, "HTTP reconnect: recovered during dialect probe, abandoning attempt");
                    self.crash_tracker.lock().await.reset();
                    break;
                }

                let outcome = self.complete_handshake(discover_result, true).await;
                drop(handshake);
                match outcome {
                    Ok(HandshakeOutcome::Completed) => {
                        info!(url = %self.config.url, "HTTP reconnect succeeded");
                        break;
                    }
                    Ok(HandshakeOutcome::Obsolete) => {
                        debug!(url = %self.config.url, "HTTP reconnect: recovered during credential loading, abandoning attempt");
                        self.crash_tracker.lock().await.reset();
                        break;
                    }
                    Err(e) => {
                        warn!(url = %self.config.url, error = %e, "HTTP reconnect attempt failed");
                    }
                }
            }
        }
    }

    /// Decide what a fired [`Self::recovered_notify`] means for the current
    /// attempt. The signal is only a hint: [`Self::note_request_success`]
    /// publishes it after releasing the `health` lock, so a flip from an
    /// earlier outage can be delivered after a LATER demotion has already
    /// consumed the reconnect permit. Acting on such a stale signal would
    /// stand the supervisor down while the adapter is `Unhealthy`, with no
    /// further permit ever coming (repeat failures while `Unhealthy` don't
    /// notify). So re-read health: `true` (adapter is no longer `Unhealthy`)
    /// means stand down — the backoff is reset for the caller to `break`;
    /// `false` means the signal was stale and the caller resumes the SAME
    /// pending phase (no attempt is recorded). The caller re-arms the hint
    /// BEFORE calling this, so a genuine flip that lands between the stale
    /// read and the re-arm is still delivered (pinned by
    /// `genuine_recovery_in_stale_signal_gap_short_circuits_backoff`).
    async fn stood_down_after_recovery_signal(&self, phase: &str) -> bool {
        if matches!(*self.health.read().await, HealthStatus::Unhealthy(_)) {
            debug!(
                url = %self.config.url,
                phase = phase,
                "HTTP reconnect: stale recovery notification while still Unhealthy, retrying"
            );
            return false;
        }
        debug!(
            url = %self.config.url,
            phase = phase,
            "HTTP reconnect: recovered reactively, abandoning attempt"
        );
        self.crash_tracker.lock().await.reset();
        true
    }

    /// Final step of a successful handshake: clear the transport-failure
    /// counter and set `Healthy` under one `health` write lock (the same
    /// discipline as [`Self::note_request_success`]), emitting one
    /// `tools_changed` tick only when this write performed the actual
    /// `Unhealthy → Healthy` flip. If a caller's request already recovered the
    /// adapter while the handshake was in flight, that path owns the tick and
    /// this one stays silent. Like the reactive flip it fires
    /// [`Self::recovered_notify`] so any other pending attempt stands down.
    async fn mark_handshake_healthy(&self) {
        let flipped = {
            let mut health = self.health.write().await;
            self.mark_healthy_locked(&mut health)
        };
        self.finish_handshake_healthy(flipped).await;
    }

    /// The `Healthy` write of [`Self::mark_handshake_healthy`], performed by
    /// the caller under the `health` write lock. Returns whether this write
    /// performed the `Unhealthy → Healthy` flip.
    fn mark_healthy_locked(&self, health: &mut HealthStatus) -> bool {
        self.transport_failures.store(0, Ordering::SeqCst);
        self.handshake_completed.store(true, Ordering::SeqCst);
        let was_unhealthy = matches!(*health, HealthStatus::Unhealthy(_));
        *health = HealthStatus::Healthy;
        if was_unhealthy {
            self.recovery_generation.fetch_add(1, Ordering::SeqCst);
        }
        was_unhealthy
    }

    /// The post-lock half of [`Self::mark_handshake_healthy`]: reset the
    /// backoff and, if `flipped`, publish the recovery.
    async fn finish_handshake_healthy(&self, flipped: bool) {
        self.crash_tracker.lock().await.reset();
        if flipped {
            self.recovered_notify.notify_waiters();
            let _ = self.tools_changed_tx.send(());
        }
    }

    /// The single commit point of the 2026 stateless path. The probe is
    /// read-only and 2026 needs no handshake, so nothing has reached the
    /// upstream yet: a supervisor attempt stays cancellable right up to here,
    /// however many awaits the path took to get here (probe, lock waits,
    /// identity validation). The currency check and every shared write —
    /// identity, dialect, session, listener, health — run under ONE `health`
    /// write lock, so a reactive recovery that lands during any of those
    /// awaits is found by the check and nothing is committed on top of it;
    /// otherwise an obsolete attempt would switch a `Healthy` adapter to
    /// sessionless 2026 formatting against the legacy session a caller just
    /// recovered, with no retry armed to undo it. The caller-owned path
    /// (`supervisor_attempt == false`) always commits.
    async fn commit_2026_handshake(
        &self,
        identity: ServerIdentity,
        supervisor_attempt: bool,
    ) -> HandshakeOutcome {
        #[cfg(test)]
        if let Some(gate) = &self.handshake_send_gate {
            gate.reached.notify_one();
            gate.release.notified().await;
        }
        let flipped = {
            let mut health = self.health.write().await;
            if supervisor_attempt && !matches!(*health, HealthStatus::Unhealthy(_)) {
                debug!(
                    url = %self.config.url,
                    "obsolete HTTP reconnect attempt: recovered before the 2026 commit, leaving state untouched"
                );
                return HandshakeOutcome::Obsolete;
            }
            self.commit_server_identity(identity).await;
            self.set_upstream_dialect(ProtocolVersion::V2026_07_28)
                .await;
            // 2026 is stateless: no notifications/initialized, no session
            // id. Discard any session left over from a previous (legacy)
            // connection so it is never echoed at the new upstream.
            *self.session_id.write().await = None;
            self.spawn_get_listener().await;
            self.mark_healthy_locked(&mut health)
        };
        self.finish_handshake_healthy(flipped).await;
        info!(url = %self.config.url, "HTTP MCP adapter initialized (2026 stateless path)");
        HandshakeOutcome::Completed
    }

    /// Set `Unhealthy(reason)` for a failed handshake step. A supervisor
    /// attempt (`supervisor_attempt == true`) is obsolete once the adapter is
    /// no longer `Unhealthy` — a caller's request recovered it while the
    /// attempt was in flight, or shutdown stopped it — and must not overwrite
    /// that state; the check and the write share one `health` write lock.
    /// The caller-owned `initialize()` path always writes. Returns whether
    /// the write happened.
    async fn set_handshake_unhealthy(&self, reason: String, supervisor_attempt: bool) -> bool {
        let mut health = self.health.write().await;
        if supervisor_attempt && !matches!(*health, HealthStatus::Unhealthy(_)) {
            debug!(
                url = %self.config.url,
                reason = %reason,
                "obsolete HTTP reconnect attempt failed; health already recovered, leaving it untouched"
            );
            return false;
        }
        *health = HealthStatus::Unhealthy(reason);
        true
    }

    /// Record a failed `initialize` exchange: [`Self::set_handshake_unhealthy`]
    /// plus the error log (skipped when the write was refused as obsolete).
    async fn fail_handshake(&self, e: &AdapterError, supervisor_attempt: bool) {
        if self
            .set_handshake_unhealthy(e.to_string(), supervisor_attempt)
            .await
        {
            error!(url = %self.config.url, error = %e, "HTTP MCP adapter initialization failed");
        }
    }
}

impl Drop for HttpAdapter {
    fn drop(&mut self) {
        // The supervisor's own handle onto the shared state must not tear
        // anything down — only the caller-owned adapter does.
        if !self.owns_background_tasks {
            return;
        }
        // Same outcome as `shutdown()` minus the joins: wake any pending
        // `shutdown.notified()` in the GET listener and the reconnect
        // supervisor so they exit at the next `tokio::select!` tick, then
        // abort both handles. The slots are `std` mutexes precisely so this
        // synchronous path can always drain them: a `spawn_get_listener`
        // critical section that already passed the `shutting_down` check
        // finishes storing its handle (microseconds, no await) and this lock
        // then finds and aborts it. The supervisor (the only producer of new
        // listeners) goes first. This matters in production because the
        // OAuth wrapper drops a superseded inner adapter without calling
        // `shutdown()`.
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
        if let Some(handle) = lock_slot(&self.reconnect_handle).take() {
            handle.abort();
        }
        if let Some(handle) = lock_slot(&self.listener_handle).take() {
            handle.abort();
        }
    }
}

impl HttpAdapter {
    /// Establish (or re-establish) the upstream session: dialect probe,
    /// `initialize` handshake (capturing a fresh `Mcp-Session-Id`),
    /// `notifications/initialized`, and the `GET` listener. On success sets
    /// `Healthy`, clears the transport-failure counter and resets the
    /// reconnect backoff. On failure sets `Unhealthy(<reason>)`.
    ///
    /// The caller-owned path used by [`McpAdapter::initialize`]. The
    /// reconnect supervisor ([`Self::run_supervisor`]) runs the same two
    /// steps itself so it can race the stateless probe against a reactive
    /// recovery before committing to [`Self::complete_handshake`].
    async fn connect_and_handshake(&self) -> Result<(), AdapterError> {
        // Discover-first dialect detection (T7/T8): probe `server/discover`
        // before the legacy handshake. A 2026 upstream answers with a
        // `protocolVersion` of `2026-07-28`, in which case the relay skips
        // the `initialize`/`notifications/initialized` handshake and the
        // `Mcp-Session-Id` machinery entirely — the 2026 transport is
        // stateless, carrying version + identity on every request instead.
        // Any other outcome (legacy result, JSON-RPC error, transport
        // failure) falls through to the unchanged legacy handshake.
        let discover_result = self.try_discover_probe().await;
        // The caller-owned path never reports `Obsolete` (it always writes).
        self.complete_handshake(discover_result, false)
            .await
            .map(|_| ())
    }

    /// Everything after the dialect probe: the 2026 stateless path or the
    /// legacy `initialize` exchange, then `notifications/initialized`, the
    /// `GET` listener and the `Healthy` flip. `supervisor_attempt` marks a
    /// reconnect-supervisor attempt, whose failure writes are conditional
    /// (see [`Self::set_handshake_unhealthy`]) and which stands down with
    /// [`HandshakeOutcome::Obsolete`] if a reactive recovery lands before
    /// its point of no return: on the legacy path the JIT credential lookup
    /// is the last await before the `initialize` POST, from which on this
    /// runs to completion so the session the upstream hands back is always
    /// the one the adapter keeps; the 2026 path never reaches the upstream
    /// and checks under the lock of its single commit
    /// ([`Self::commit_2026_handshake`]). Takes `&self` so the supervisor's
    /// [`Self::task_clone`] can drive it.
    async fn complete_handshake(
        &self,
        discover_result: Option<Value>,
        supervisor_attempt: bool,
    ) -> Result<HandshakeOutcome, AdapterError> {
        {
            if detect_upstream_dialect(discover_result.as_ref(), None).is_2026() {
                let result = discover_result.as_ref().expect(
                    "detect_upstream_dialect reports 2026 only when a discover result is present",
                );
                // Validate the discovery result BEFORE publishing the
                // dialect: like the session commit below, a dialect this
                // attempt did not successfully negotiate must never reach the
                // shared state. Otherwise an obsolete supervisor attempt that
                // rejects an invalid 2026 response would leave a `Healthy`
                // adapter (recovered reactively on its legacy session)
                // formatting every later request as sessionless 2026 traffic.
                let identity = self
                    .validate_server_identity(result, supervisor_attempt)
                    .await?;
                return Ok(self
                    .commit_2026_handshake(identity, supervisor_attempt)
                    .await);
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
            let (result, new_session_id): (Value, Option<String>) = {
                let id = self.next_id();
                let request = jsonrpc::new_request("initialize", Some(params), id);
                trace!(method = "initialize", id = id, url = %self.config.url, "sending HTTP JSON-RPC request");
                let builder = self.client.post(&self.config.url).json(&request);
                let builder = self.apply_jit_bearer(builder).await;
                #[cfg(test)]
                if let Some(gate) = &self.handshake_send_gate {
                    gate.reached.notify_one();
                    gate.release.notified().await;
                }
                // The credential lookup above is asynchronous (it may load
                // the token manager): caller traffic can recover the adapter
                // while it yields. A supervisor attempt is still cancellable
                // here — nothing has reached the upstream — so re-check once
                // more before the POST rather than rotate a recovered session.
                if supervisor_attempt
                    && !matches!(*self.health.read().await, HealthStatus::Unhealthy(_))
                {
                    return Ok(HandshakeOutcome::Obsolete);
                }
                let send_result = builder.send().await.map_err(|e| {
                    if e.is_timeout() {
                        AdapterError::Timeout(self.config.timeout_secs)
                    } else if e.is_connect() {
                        AdapterError::ConnectionFailed(with_local_network_hint(
                            &self.config.url,
                            connect_error_message(&self.config.url, &e),
                        ))
                    } else {
                        AdapterError::HttpError {
                            status: 0,
                            body: format_error_chain(&e),
                        }
                    }
                });
                let resp = match send_result {
                    Ok(r) => r,
                    Err(e) => {
                        self.fail_handshake(&e, supervisor_attempt).await;
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
                    self.fail_handshake(&e, supervisor_attempt).await;
                    return Err(e);
                }

                // Capture `Mcp-Session-Id` from the initialize response BEFORE
                // we consume the body. Reqwest's `HeaderMap` lookups are
                // case-insensitive, so this matches whatever spelling the
                // upstream sends (Mcp-Session-Id, mcp-session-id, etc.).
                //
                // The header is only STAGED here: a 2xx status says nothing
                // about the JSON-RPC body, which may still be an error or
                // malformed. The shared session is committed below, once the
                // result and server identity have validated — otherwise an
                // obsolete supervisor attempt that fails at the body stage
                // would wipe the working session a caller's request has just
                // recovered, leaving a `Healthy` adapter whose every request
                // gets 400 and no path back.
                let new_session_id = resp
                    .headers()
                    .get(&MCP_SESSION_ID_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                match &new_session_id {
                    Some(sid) => debug!(
                        session_id = %sid,
                        "captured Mcp-Session-Id from initialize response"
                    ),
                    None => debug!(
                        "initialize response carried no Mcp-Session-Id; session is stateless"
                    ),
                }

                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let response: JsonRpcResponse = if content_type.contains("text/event-stream") {
                    trace!(
                        id = id,
                        "response is SSE (text/event-stream), parsing events"
                    );
                    let body = match resp.text().await {
                        Ok(b) => b,
                        Err(e) => {
                            let err = AdapterError::ProtocolError(format!(
                                "failed to read SSE body: {}",
                                e
                            ));
                            self.fail_handshake(&err, supervisor_attempt).await;
                            return Err(err);
                        }
                    };
                    match Self::parse_sse_response(&body, id, Some(&self.tools_changed_tx)) {
                        Ok(r) => r,
                        Err(e) => {
                            self.fail_handshake(&e, supervisor_attempt).await;
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
                            self.fail_handshake(&err, supervisor_attempt).await;
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
                    self.fail_handshake(&e, supervisor_attempt).await;
                    return Err(e);
                }

                match response.result {
                    Some(v) => (v, new_session_id),
                    None => {
                        let err = AdapterError::ProtocolError("response has no result".into());
                        self.fail_handshake(&err, supervisor_attempt).await;
                        return Err(err);
                    }
                }
            };

            // Validate + record the upstream serverInfo.name (REQUIRED per MCP
            // spec enforcement). Shared with the 2026 stateless path above.
            self.apply_server_identity(&result, supervisor_attempt)
                .await?;

            // The handshake is valid: commit the session. Every successful
            // `initialize` REPLACES the stored session state — an upstream
            // that restarted between connections issues a new id (or none at
            // all if it came back stateless), and echoing the old one would
            // get every later request rejected. This precedes
            // `notifications/initialized` and the GET listener, both of which
            // read the stored id.
            *self.session_id.write().await = new_session_id;

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

            self.mark_handshake_healthy().await;
            info!(url = %self.config.url, "HTTP MCP adapter initialized");
            Ok(HandshakeOutcome::Completed)
        }
    }
}

#[async_trait]
impl McpAdapter for HttpAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        async {
            // Re-arm after a previous `shutdown()`: that path latches
            // `shutting_down` so no listener can be installed behind its
            // drain. A caller-owned initialize is exclusive (`&mut self`) and
            // shutdown has already aborted and joined the old supervisor and
            // listener, so clearing the latch here cannot race either of them.
            self.shutting_down.store(false, Ordering::SeqCst);
            // A live supervisor (this is a re-initialize of an enabled
            // endpoint, e.g. the management enable path) may have a reconnect
            // handshake in flight on its `task_clone`. Wait for it rather
            // than racing it for the session id (see `handshake_lock`); the
            // `Starting` write and our own handshake then run with no other
            // handshake able to interleave.
            let outcome = {
                let _handshake = self.handshake_lock.lock().await;
                *self.health.write().await = HealthStatus::Starting;
                self.connect_and_handshake().await
            };
            if let Err(e) = outcome {
                // Same retry contract as a successful start: the adapter is
                // `Unhealthy(<init error>)` and the supervisor brings it back
                // once the upstream is reachable. Every `initialize()` caller
                // gets this — including the management enable path, which
                // re-initializes after `shutdown()` and propagates the error
                // with `?`. The OAuth wrapper drops a failed inner adapter,
                // whose `Drop` aborts the supervisor again, so its behaviour
                // is unchanged.
                self.retry_initialize_in_background().await;
                return Err(e);
            }
            // Arm the reconnect supervisor now so a later transport-dead
            // demotion recovers without waiting for caller traffic.
            self.ensure_supervisor_running().await;
            Ok(())
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        async {
            // Never-initialized and down: answer like a `FailedAdapter` so a
            // catalog rebuild neither waits on a dead upstream nor recovers
            // the adapter without a session. The supervisor owns recovery.
            if !self.handshake_completed.load(Ordering::SeqCst)
                && matches!(*self.health.read().await, HealthStatus::Unhealthy(_))
            {
                return Ok(vec![]);
            }
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

    async fn list_resources(&self) -> Result<Vec<Value>, AdapterError> {
        async {
            let result = self.send_request("resources/list", None).await?;
            match result.get("resources") {
                Some(Value::Array(items)) => Ok(items.clone()),
                _ => Ok(vec![]),
            }
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_resource_templates(&self) -> Result<Vec<Value>, AdapterError> {
        async {
            let result = self.send_request("resources/templates/list", None).await?;
            match result.get("resourceTemplates") {
                Some(Value::Array(items)) => Ok(items.clone()),
                _ => Ok(vec![]),
            }
        }
        .instrument(self.span.clone())
        .await
    }

    async fn read_resource(&self, uri: &str) -> Result<Value, AdapterError> {
        async {
            let params = serde_json::json!({ "uri": uri });
            self.send_request("resources/read", Some(params)).await
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_prompts(&self) -> Result<Vec<Value>, AdapterError> {
        async {
            let result = self.send_request("prompts/list", None).await?;
            match result.get("prompts") {
                Some(Value::Array(items)) => Ok(items.clone()),
                _ => Ok(vec![]),
            }
        }
        .instrument(self.span.clone())
        .await
    }

    async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, AdapterError> {
        async {
            let mut params = serde_json::Map::new();
            params.insert("name".to_string(), Value::String(name.to_string()));
            if let Some(args) = arguments {
                params.insert("arguments".to_string(), args);
            }
            self.send_request("prompts/get", Some(Value::Object(params)))
                .await
        }
        .instrument(self.span.clone())
        .await
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
                // The overlay displays the event's `server_name`, so prefer
                // the effective (override-aware) name over the raw
                // upstream-derived one, matching the observability capture.
                let server_type = self.server_type.read().await.clone();
                let server_name = match server_type.clone() {
                    Some(name) => Some(name),
                    None => self.upstream_server_name.read().await.clone(),
                };
                bus.send(ToolCallEvent::Started {
                    request_id: request_id.clone(),
                    request_uid: span_ctx.request_uid.clone(),
                    ts: iso8601_now(),
                    endpoint: self.config.endpoint_name.clone(),
                    transport: "http".into(),
                    server_type,
                    server_name,
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
            // JIT 401 interception: swallow a hard 401 + Bearer challenge and
            // self-initiate OAuth instead of forwarding it downstream. No-op
            // unless a JIT interceptor is attached (none are today).
            let result = self.maybe_intercept_401(result).await;
            let duration_ms = start.elapsed().as_millis();
            // A transport-level `Ok` can still carry a tool-level error
            // envelope (`{ content: [...], isError: true }`). Surface it as a
            // failed call in the activity log, tracing line, and overlay
            // events — mirroring the registry's durable capture — while
            // forwarding the envelope to the client unchanged.
            let tool_error = result
                .as_ref()
                .ok()
                .and_then(crate::adapter::tool_result_error_message);
            let now = chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            let log_line = match (&result, &tool_error) {
                (Ok(_), None) => format!(
                    "{}  INFO call_tool tool={} status=ok duration={}ms",
                    now, name, duration_ms
                ),
                (Ok(_), Some(msg)) => format!(
                    "{}  WARN call_tool tool={} status=error duration={}ms error={}",
                    now, name, duration_ms, msg
                ),
                (Err(e), _) => format!(
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
            match (&result, &tool_error) {
                (Ok(_), None) => tracing::info!(
                    tool = %name,
                    status = "ok",
                    duration_ms = duration_ms,
                    client_name = ?client_name,
                    client_version = ?client_version,
                    "Tool call completed"
                ),
                (Ok(_), Some(msg)) => tracing::warn!(
                    tool = %name,
                    status = "error",
                    duration_ms = duration_ms,
                    error = %msg,
                    client_name = ?client_name,
                    client_version = ?client_version,
                    "Tool call failed"
                ),
                (Err(e), _) => tracing::warn!(
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
                match (&result, &tool_error) {
                    (Ok(_), None) => bus.send(ToolCallEvent::Completed {
                        request_id,
                        ts,
                        duration_ms: duration_ms_u64,
                        status: "ok".into(),
                    }),
                    (Ok(_), Some(msg)) => bus.send(ToolCallEvent::Failed {
                        request_id,
                        ts,
                        duration_ms: duration_ms_u64,
                        status: "error".into(),
                        error_message: msg.clone(),
                    }),
                    (Err(e), _) => bus.send(ToolCallEvent::Failed {
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
            // Signal the GET listener and the reconnect supervisor to exit at
            // the next select tick, then await their handles so they tear
            // down before the adapter does. The supervisor is the only thing
            // that can install a NEW listener (via a reconnect handshake), so
            // it is stopped and joined first; only then is the listener slot
            // drained, guaranteeing no listener is spawned after the drain.
            self.shutting_down.store(true, Ordering::SeqCst);
            self.shutdown_notify.notify_waiters();
            // Take each handle out of its slot before awaiting it so the
            // `std` slot guard is never held across an await.
            let supervisor = lock_slot(&self.reconnect_handle).take();
            if let Some(handle) = supervisor {
                handle.abort();
                let _ = handle.await;
            }
            let listener = lock_slot(&self.listener_handle).take();
            if let Some(handle) = listener {
                handle.abort();
                let _ = handle.await;
            }
            // The session listener is gone: until a handshake respawns it,
            // the adapter is back to the never-initialized contract (see
            // `handshake_completed`). Otherwise a re-`initialize()` that
            // fails, followed by a stray success on the old session during
            // the supervisor's backoff, would promote to `Healthy` and stop
            // the retries with no listener to carry tool changes.
            self.handshake_completed.store(false, Ordering::SeqCst);
            *self.health.write().await = HealthStatus::Stopped;
            info!(url = %self.config.url, "HTTP MCP adapter shut down");
            Ok(())
        }
        .instrument(self.span.clone())
        .await
    }

    /// Surface the current `Unhealthy` reason as an `[ERROR]` log line, the
    /// same shape a `FailedAdapter` reports, so the endpoint logs view keeps
    /// showing why the upstream is down while the supervisor retries.
    async fn stderr_lines(&self) -> Vec<String> {
        match &*self.health.read().await {
            HealthStatus::Unhealthy(reason) => vec![format!("[ERROR] {}", reason)],
            _ => vec![],
        }
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

    /// Repeated `initialize` calls (standalone HTTP/SSE reconnects) must NOT
    /// re-append `server_type` to the per-endpoint span's field list. The
    /// once-guard flips on the first record and short-circuits every later
    /// call, preventing unbounded `endpoint{…}` log-header growth.
    #[test]
    fn record_server_type_once_guards_repeated_calls() {
        let adapter = HttpAdapter::new(HttpConfig::new("http://localhost:8080/mcp"));
        assert!(!adapter.server_type_recorded_flag());

        adapter.record_server_type_once("some-server");
        assert!(adapter.server_type_recorded_flag());

        // Second and subsequent calls (e.g. after a reconnect re-runs
        // initialize) must NOT re-record — the guard has already flipped,
        // so this is a no-op.
        adapter.record_server_type_once("some-server");
        adapter.record_server_type_once("other-name");
        assert!(adapter.server_type_recorded_flag());
    }

    /// An inner adapter built for wrapping (`new_with_client_inner`) shares
    /// the wrapper's `endpoint` span, so its `server_type` once-guard must be
    /// pre-flipped: the wrapper owns that record behind its own once-guard,
    /// and inner adapters are rebuilt on every token swap — a fresh guard
    /// would re-append `server_type` to the shared span on each rebuild.
    #[test]
    fn new_with_client_inner_preflips_server_type_guard() {
        let span = tracing::info_span!(
            "endpoint",
            endpoint = "test",
            transport = "oauth",
            server_type = tracing::field::Empty,
        );
        let adapter = HttpAdapter::new_with_client_inner(
            HttpConfig::new("http://localhost:8080/mcp"),
            Client::new(),
            span,
        );
        assert!(adapter.server_type_recorded_flag());

        // Recording is a no-op — the guard never resets.
        adapter.record_server_type_once("some-server");
        assert!(adapter.server_type_recorded_flag());
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
            let mut guard = lock_slot(&adapter.listener_handle);
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
            let mut guard = lock_slot(&adapter.listener_handle);
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

        // State machine advanced and the URL is also stored on the interceptor.
        assert_eq!(
            interceptor.state().await,
            crate::adapter::oauth::OAuthState::NeedsLogin
        );
        let authorize_url = interceptor
            .pending_authorize_url()
            .await
            .expect("pending authorize URL stored on the interceptor");

        // The raw upstream 401 / WWW-Authenticate challenge is never leaked.
        // Remove only the exact stored authorize URL first: its arbitrary
        // digits (ephemeral fixture port, state, code_challenge) can
        // spuriously contain "401", but everything else in the surfaced text
        // must stay free of the upstream challenge.
        let without_url = text.replace(&authorize_url, "");
        assert!(!without_url.contains("401"));
        assert!(!without_url.contains("WWW-Authenticate"));

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

    /// Fixture that gates EVERY path on `Authorization: Bearer <expected>`:
    /// `server/discover` (answered as legacy), `initialize` (issues a
    /// session), notifications (202), `tools/list`, and the `GET` SSE stream
    /// (which emits one `tools/list_changed` per accepted connection). Any
    /// request without the current bearer gets a 401 and is counted in
    /// `unauthenticated`, so a test can prove the token reached every wire
    /// path. `expected` is swappable so token rotation can be exercised.
    #[derive(Clone)]
    struct BearerEverywhereFixture {
        expected: Arc<std::sync::Mutex<String>>,
        unauthenticated: Arc<AtomicU64>,
        discover_authed: Arc<AtomicU64>,
        init_authed: Arc<AtomicU64>,
        notification_authed: Arc<AtomicU64>,
        get_authed: Arc<AtomicU64>,
        session: Arc<std::sync::Mutex<Option<String>>>,
    }

    impl BearerEverywhereFixture {
        fn rotate_expected(&self, token: &str) {
            *self.expected.lock().unwrap() = token.to_string();
        }
    }

    async fn bearer_everywhere_handler(
        State(fx): State<BearerEverywhereFixture>,
        req: axum::extract::Request,
    ) -> axum::response::Response {
        let expected = format!("Bearer {}", fx.expected.lock().unwrap());
        let authed = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == expected)
            .unwrap_or(false);
        if !authed {
            fx.unauthenticated.fetch_add(1, Ordering::SeqCst);
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    axum::http::header::WWW_AUTHENTICATE,
                    "Bearer realm=\"Test\"",
                )],
                "unauthorized",
            )
                .into_response();
        }
        if req.method() == axum::http::Method::GET {
            fx.get_authed.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(8);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(Event::default().data(
                        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}",
                    )))
                    .await;
                tx.closed().await;
            });
            return Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
                .keep_alive(KeepAlive::default())
                .into_response();
        }
        let session_header = req
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body_bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let value: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
        let id = value["id"].as_u64().unwrap_or(0);
        if value["method"] == "server/discover" {
            fx.discover_authed.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "jsonrpc": "2.0",
                "error": {"code": -32601, "message": "method not found"},
                "id": id,
            }))
            .into_response();
        }
        if value["method"] == "initialize" {
            let n = fx.init_authed.fetch_add(1, Ordering::SeqCst) + 1;
            let sid = format!("bearer-sess-{}", n);
            *fx.session.lock().unwrap() = Some(sid.clone());
            let mut resp = Json(json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "bearer-http", "version": "0.0.0"}
                },
                "id": id,
            }))
            .into_response();
            resp.headers_mut().insert(
                MCP_SESSION_ID_HEADER.clone(),
                reqwest::header::HeaderValue::from_str(&sid).unwrap(),
            );
            return resp;
        }
        if session_header != *fx.session.lock().unwrap() {
            return (StatusCode::BAD_REQUEST, "stale or missing session").into_response();
        }
        if value.get("id").is_none() {
            fx.notification_authed.fetch_add(1, Ordering::SeqCst);
            return (StatusCode::ACCEPTED, "").into_response();
        }
        if value["method"] == "tools/list" {
            return Json(json!({
                "jsonrpc": "2.0",
                "result": {"tools": [
                    {"name": "ping", "description": "p", "inputSchema": {"type": "object"}}
                ]},
                "id": id,
            }))
            .into_response();
        }
        Json(json!({"jsonrpc": "2.0", "result": {"ok": true}, "id": id})).into_response()
    }

    async fn start_bearer_everywhere_fixture(
        token: &str,
    ) -> (String, BearerEverywhereFixture, JoinHandle<()>) {
        let fx = BearerEverywhereFixture {
            expected: Arc::new(std::sync::Mutex::new(token.to_string())),
            unauthenticated: Arc::new(AtomicU64::new(0)),
            discover_authed: Arc::new(AtomicU64::new(0)),
            init_authed: Arc::new(AtomicU64::new(0)),
            notification_authed: Arc::new(AtomicU64::new(0)),
            get_authed: Arc::new(AtomicU64::new(0)),
            session: Arc::new(std::sync::Mutex::new(None)),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/mcp", any(bearer_everywhere_handler))
            .with_state(fx.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}/mcp", addr), fx, handle)
    }

    async fn persist_bearer(tm: &crate::token_manager::TokenManager, endpoint: &str, token: &str) {
        use crate::token_manager::TokenSet;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        tm.save(
            endpoint,
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
    }

    /// PR #163 review (round 5): the persisted JIT bearer must reach EVERY
    /// request the adapter puts on the wire — the `server/discover` probe,
    /// the `initialize` POST, `notifications/initialized`, tool requests and
    /// the `GET` SSE listener — on the initial handshake AND on every
    /// supervisor reconnect, read live so a rotated token is picked up by
    /// the respawned handshake and listener. Before the fix only
    /// `send_request_inner` injected it: against a fully gated upstream the
    /// handshake and listener went out unauthenticated.
    #[tokio::test]
    async fn jit_bearer_reaches_handshake_notifications_and_get_listener() {
        use crate::adapter::oauth::jit::JitInterceptor;
        use crate::oauth::OAuthFlowManager;
        use crate::token_manager::TokenManager;

        let (url, fx, server) = start_bearer_everywhere_fixture("token-v1").await;
        let tmp = tempfile::tempdir().unwrap();
        let tm = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        persist_bearer(&tm, "ep-everywhere", "token-v1").await;
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let interceptor = Arc::new(JitInterceptor::new(9400, flow_mgr, Some(tm.clone()), true));

        let mut config = HttpConfig::new(url);
        config.endpoint_name = "ep-everywhere".to_string();
        let mut adapter = HttpAdapter::new(config);
        adapter.set_jit_interceptor(interceptor.clone());
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        adapter
            .initialize()
            .await
            .expect("fully gated upstream must accept the authenticated handshake");
        use_fast_backoff(&adapter).await;
        assert_eq!(fx.discover_authed.load(Ordering::SeqCst), 1);
        assert_eq!(fx.init_authed.load(Ordering::SeqCst), 1);
        assert!(fx.notification_authed.load(Ordering::SeqCst) >= 1);
        // The listener's stream was accepted and delivered its notification.
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("authenticated GET listener must deliver list_changed")
            .unwrap();
        assert_eq!(fx.get_authed.load(Ordering::SeqCst), 1);
        adapter
            .list_tools()
            .await
            .expect("tools/list authenticated");

        // Rotate the token: the reconnect must read the NEW bearer live on
        // every path, not a value captured when the adapter was built.
        fx.rotate_expected("token-v2");
        persist_bearer(&tm, "ep-everywhere", "token-v2").await;
        demote_via_transport_failures(&adapter).await;
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(
            recovered,
            "supervisor must reconnect with the rotated bearer"
        );
        assert_eq!(fx.discover_authed.load(Ordering::SeqCst), 2);
        assert_eq!(fx.init_authed.load(Ordering::SeqCst), 2);
        assert!(fx.notification_authed.load(Ordering::SeqCst) >= 2);
        assert!(
            wait_until(Duration::from_secs(2), || {
                fx.get_authed.load(Ordering::SeqCst) == 2
            })
            .await,
            "the respawned GET listener must carry the rotated bearer"
        );
        adapter
            .list_tools()
            .await
            .expect("tools/list with rotated bearer");

        assert_eq!(
            fx.unauthenticated.load(Ordering::SeqCst),
            0,
            "no request on any path may reach the upstream without the current bearer"
        );
        assert_ne!(
            interceptor.state().await,
            crate::adapter::oauth::OAuthState::NeedsLogin,
            "an authenticated adapter never re-triggers the JIT flow"
        );

        adapter.shutdown().await.unwrap();
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

    // --- isError tool results surfaced as failures ---

    /// Fixture whose `POST /mcp` answers every JSON-RPC request with the given
    /// `result` payload. Minimal enough for direct `call_tool` tests — the
    /// application/json response path does not validate the JSON-RPC id.
    async fn spawn_fixed_result_fixture(
        result: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::post;
        use axum::{Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let router = Router::new().route(
            "/mcp",
            post(move || {
                let result = result.clone();
                async move { Json(json!({"jsonrpc": "2.0", "result": result, "id": 1})) }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (base, handle)
    }

    /// A transport-level `Ok` whose envelope carries `isError: true` must be
    /// surfaced as a FAILED call: the activity log records `status=error` with
    /// the captured message, and the event bus receives `Started` → `Failed`
    /// (not `Completed`), matching the registry's durable capture. The
    /// envelope itself is still forwarded unchanged.
    #[tokio::test]
    async fn call_tool_iserror_envelope_logs_error_and_emits_failed_event() {
        let (base, server) = spawn_fixed_result_fixture(json!({
            "content": [{"type": "text", "text": "invalid_grant"}],
            "isError": true,
        }))
        .await;
        let adapter = HttpAdapter::new(HttpConfig::new(format!("{}/mcp", base)));
        let bus = ToolCallEventBus::with_default_capacity();
        adapter.set_event_bus(bus.clone());
        let mut rx = bus.subscribe();

        let value = adapter
            .call_tool("search", json!({}))
            .await
            .expect("isError envelope is still a transport-level Ok");
        assert_eq!(value["isError"], true, "envelope forwarded unchanged");

        let log = adapter.activity_log().await;
        let line = log.last().expect("activity log line recorded");
        assert!(line.contains("status=error"), "got: {line}");
        assert!(line.contains("invalid_grant"), "got: {line}");

        match rx.try_recv().expect("started event must be buffered") {
            ToolCallEvent::Started { .. } => {}
            other => panic!("expected Started, got {other:?}"),
        }
        match rx.try_recv().expect("terminal event must be buffered") {
            ToolCallEvent::Failed {
                status,
                error_message,
                ..
            } => {
                assert_eq!(status, "error");
                assert_eq!(error_message, "invalid_grant");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        server.abort();
    }

    /// A plain success envelope (`isError` absent) keeps the pre-existing
    /// behavior: `status=ok` activity-log line and a `Completed` event.
    #[tokio::test]
    async fn call_tool_success_envelope_logs_ok_and_emits_completed_event() {
        let (base, server) = spawn_fixed_result_fixture(json!({
            "content": [{"type": "text", "text": "all good"}],
        }))
        .await;
        let adapter = HttpAdapter::new(HttpConfig::new(format!("{}/mcp", base)));
        let bus = ToolCallEventBus::with_default_capacity();
        adapter.set_event_bus(bus.clone());
        let mut rx = bus.subscribe();

        adapter
            .call_tool("search", json!({}))
            .await
            .expect("success envelope");

        let log = adapter.activity_log().await;
        let line = log.last().expect("activity log line recorded");
        assert!(line.contains("status=ok"), "got: {line}");

        match rx.try_recv().expect("started event must be buffered") {
            ToolCallEvent::Started { .. } => {}
            other => panic!("expected Started, got {other:?}"),
        }
        match rx.try_recv().expect("terminal event must be buffered") {
            ToolCallEvent::Completed { status, .. } => assert_eq!(status, "ok"),
            other => panic!("expected Completed, got {other:?}"),
        }
        server.abort();
    }

    // --- Reactive transport-failure health detection ---

    /// Classification is the contract the reactive counter relies on: only
    /// transport-dead signals (connect/timeout/`HttpError { status: 0 }`) feed
    /// the counter; a live-but-erroring server (non-zero HTTP status, JSON-RPC
    /// error, protocol error) must never be treated as dead.
    #[test]
    fn is_transport_dead_classifies_errors() {
        assert!(HttpAdapter::is_transport_dead(
            &AdapterError::ConnectionFailed("refused".into())
        ));
        assert!(HttpAdapter::is_transport_dead(&AdapterError::Timeout(30)));
        assert!(HttpAdapter::is_transport_dead(&AdapterError::HttpError {
            status: 0,
            body: "send error".into(),
        }));

        assert!(!HttpAdapter::is_transport_dead(&AdapterError::HttpError {
            status: 401,
            body: "unauthorized".into(),
        }));
        assert!(!HttpAdapter::is_transport_dead(&AdapterError::HttpError {
            status: 500,
            body: "boom".into(),
        }));
        assert!(!HttpAdapter::is_transport_dead(
            &AdapterError::JsonRpcError {
                code: -32000,
                message: "nope".into(),
                data: None,
            }
        ));
        assert!(!HttpAdapter::is_transport_dead(
            &AdapterError::ProtocolError("bad".into())
        ));
    }

    /// A plain HTTP server that dies after init: once the upstream stops
    /// answering, repeated `call_tool` transport failures flip health to
    /// `Unhealthy` at the threshold (and not before).
    #[tokio::test]
    async fn transport_failures_flip_health_to_unhealthy() {
        // Bind then immediately drop a listener to obtain a port that refuses
        // connections — every request will fail with `ConnectionFailed`.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{}/mcp", addr);

        let adapter = HttpAdapter::new(HttpConfig::new(url));
        // Simulate the post-init state: the server came up Healthy.
        *adapter.health.write().await = HealthStatus::Healthy;

        for _ in 0..(TRANSPORT_FAILURE_THRESHOLD - 1) {
            let result = adapter.call_tool("x", json!({})).await;
            assert!(result.is_err(), "dead upstream should error");
            assert_eq!(
                adapter.health(),
                HealthStatus::Healthy,
                "health must stay Healthy below the failure threshold"
            );
        }

        let result = adapter.call_tool("x", json!({})).await;
        assert!(result.is_err());
        match adapter.health() {
            HealthStatus::Unhealthy(msg) => assert_eq!(msg, "upstream unreachable"),
            other => panic!("expected Unhealthy after threshold, got {:?}", other),
        }
    }

    /// A live-but-erroring server (hard 401 on every call) must NOT flip health:
    /// the upstream is reachable, so the transport-failure counter stays at 0.
    #[tokio::test]
    async fn http_error_does_not_flip_health() {
        let (base, server) = spawn_gated_mcp_fixture().await;
        let adapter = HttpAdapter::new(HttpConfig::new(format!("{}/mcp", base)));
        *adapter.health.write().await = HealthStatus::Healthy;

        for _ in 0..(TRANSPORT_FAILURE_THRESHOLD + 2) {
            let result = adapter.call_tool("search", json!({})).await;
            match result {
                Err(AdapterError::HttpError { status: 401, .. }) => {}
                other => panic!("expected HttpError {{ 401 }}, got {:?}", other),
            }
        }

        assert_eq!(
            adapter.health(),
            HealthStatus::Healthy,
            "a reachable server returning 401 must not be demoted"
        );
        assert_eq!(
            adapter.transport_failures.load(Ordering::SeqCst),
            0,
            "non-transport errors must not increment the failure counter"
        );

        server.abort();
    }

    /// Auto-recovery: after the adapter has demoted itself to `Unhealthy`, the
    /// next successful request flips it back to `Healthy` and resets the counter.
    #[tokio::test]
    async fn success_recovers_health_and_resets_counter() {
        // GET 405 so the test doesn't depend on the SSE channel; POST returns ok.
        let (url, server) = start_fake_http_server(true).await;
        let adapter = HttpAdapter::new(HttpConfig::new(url));
        // Simulate a prior handshake followed by a run of transport failures
        // that demoted the adapter.
        adapter.handshake_completed.store(true, Ordering::SeqCst);
        *adapter.health.write().await = HealthStatus::Unhealthy("upstream unreachable".into());
        adapter.transport_failures.store(5, Ordering::SeqCst);

        let result = adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server should answer successfully");
        assert_eq!(result["ok"], true);

        assert_eq!(
            adapter.health(),
            HealthStatus::Healthy,
            "a successful request must recover health from Unhealthy"
        );
        assert_eq!(
            adapter.transport_failures.load(Ordering::SeqCst),
            0,
            "a successful request must reset the failure counter"
        );

        server.abort();
    }

    /// Reactive-health recovery must emit a `tools_changed` tick so the
    /// registry invalidates the merged catalog that may have been rebuilt
    /// without this endpoint's tools during the outage — and ONLY on the
    /// actual Unhealthy→Healthy flip, not on every success.
    #[tokio::test]
    async fn recovery_flip_emits_tools_changed_tick_once() {
        // GET 405 so the test doesn't depend on the SSE channel; POST returns ok.
        let (url, server) = start_fake_http_server(true).await;
        let adapter = HttpAdapter::new(HttpConfig::new(url));
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");
        // Simulate a prior handshake followed by a run of transport failures
        // that demoted the adapter.
        adapter.handshake_completed.store(true, Ordering::SeqCst);
        *adapter.health.write().await = HealthStatus::Unhealthy("upstream unreachable".into());
        adapter.transport_failures.store(5, Ordering::SeqCst);

        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server should answer successfully");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(
            rx.try_recv().is_ok(),
            "Unhealthy→Healthy flip must emit a tools_changed tick"
        );
        assert!(
            rx.try_recv().is_err(),
            "the flip must emit exactly one tick"
        );

        // A further success while already Healthy must NOT tick.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server should answer successfully");
        assert!(
            rx.try_recv().is_err(),
            "a success while already Healthy must not emit a tick"
        );

        server.abort();
    }

    /// Fixture for the reconnect-supervisor tests: a legacy Streamable HTTP
    /// upstream whose POST side can be switched off (every POST → 503, so a
    /// handshake attempt fails) and back on. Each successful `initialize`
    /// issues a fresh `Mcp-Session-Id` (`sess-<n>`); every non-initialize
    /// request must echo the most recently issued id or gets 400, mirroring
    /// a real upstream that forgot the old session across a restart. GET is
    /// always 405 so the tests don't depend on the SSE channel.
    ///
    /// `issue_session` off simulates an upstream that came back stateless
    /// (no `Mcp-Session-Id` on `initialize`, and requests must NOT carry one).
    /// `discover_2026` makes `server/discover` answer as a 2026 stateless
    /// upstream (the adapter then skips `initialize` entirely) instead of the
    /// default legacy method-not-found.
    /// `init_delay_ms` holds every `initialize` response for that long so a
    /// test can act while a handshake is in flight; `init_count` is bumped
    /// when the handshake STARTS so the test can detect that window.
    ///
    /// Hold/release knobs for the obsolete-attempt races: `hold_discover`
    /// (one-shot) parks the next `server/discover`, `hold_init` (one-shot)
    /// parks the next `initialize` and then answers it normally, `fail_init`
    /// parks every `initialize` and then fails it with the configured
    /// [`HeldInitFailure`]. A parked request signals `started` and waits for
    /// `release`, so a test can order itself against an in-flight handshake
    /// without wall-clock sleeps. `session_on_arrival` makes the upstream
    /// honour the session of the most recently RECEIVED `initialize` (issued
    /// when the request arrives, as a real server does) instead of the most
    /// recently answered one, so overlapping handshakes whose responses
    /// complete out of order leave the adapter on a session the upstream
    /// has already replaced.
    #[derive(Clone)]
    struct SupervisorFixture {
        accepting: Arc<AtomicBool>,
        issue_session: Arc<AtomicBool>,
        session_on_arrival: Arc<AtomicBool>,
        discover_2026: Arc<AtomicBool>,
        init_delay_ms: Arc<AtomicU64>,
        init_count: Arc<AtomicU64>,
        get_count: Arc<AtomicU64>,
        tools_list_count: Arc<AtomicU64>,
        discover_count: Arc<AtomicU64>,
        current_session: Arc<std::sync::Mutex<Option<String>>>,
        hold_discover: Arc<AtomicBool>,
        hold_init: Arc<AtomicBool>,
        fail_init: Arc<std::sync::Mutex<Option<HeldInitFailure>>>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    /// How a parked `initialize` (see `SupervisorFixture::fail_init`) fails
    /// once released. The two `Ok` variants answer HTTP 200 *with* a fresh
    /// `Mcp-Session-Id` header but a body that does not validate — exactly
    /// the window in which the header has been received but the handshake is
    /// not a success. The upstream keeps honouring the previous session (a
    /// rejected handshake creates none), so adopting the header would strand
    /// the adapter on an id every later request gets 400 for.
    #[derive(Clone, Copy, Debug)]
    enum HeldInitFailure {
        /// HTTP 503 with a plain-text body (no session header).
        Http503,
        /// HTTP 200 + session header, body is a JSON-RPC error object.
        JsonRpcError,
        /// HTTP 200 + session header, body is not JSON-RPC at all.
        MalformedBody,
    }

    impl SupervisorFixture {
        fn fail_init_with(&self, failure: Option<HeldInitFailure>) {
            *self.fail_init.lock().unwrap() = failure;
        }
    }

    async fn start_supervisor_fixture() -> (String, SupervisorFixture, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        start_supervisor_fixture_on(listener).await
    }

    /// Serve the supervisor fixture on an already-bound listener so a test
    /// can reserve a port, keep it dead for a while, then bring the upstream
    /// up at the very address the adapter was configured with.
    async fn start_supervisor_fixture_on(
        listener: TcpListener,
    ) -> (String, SupervisorFixture, JoinHandle<()>) {
        let fx = SupervisorFixture {
            accepting: Arc::new(AtomicBool::new(true)),
            issue_session: Arc::new(AtomicBool::new(true)),
            session_on_arrival: Arc::new(AtomicBool::new(false)),
            discover_2026: Arc::new(AtomicBool::new(false)),
            init_delay_ms: Arc::new(AtomicU64::new(0)),
            init_count: Arc::new(AtomicU64::new(0)),
            get_count: Arc::new(AtomicU64::new(0)),
            tools_list_count: Arc::new(AtomicU64::new(0)),
            discover_count: Arc::new(AtomicU64::new(0)),
            current_session: Arc::new(std::sync::Mutex::new(None)),
            hold_discover: Arc::new(AtomicBool::new(false)),
            hold_init: Arc::new(AtomicBool::new(false)),
            fail_init: Arc::new(std::sync::Mutex::new(None)),
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let app = Router::new()
            .route("/mcp", any(supervisor_fixture_handler))
            .with_state(fx.clone());
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}/mcp", addr), fx, handle)
    }

    /// Reserve a loopback port and release it, returning the address as an
    /// `/mcp` URL that refuses connections until a test binds it again.
    async fn reserve_dead_upstream() -> (std::net::SocketAddr, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        (addr, format!("http://{}/mcp", addr))
    }

    async fn supervisor_fixture_handler(
        State(fx): State<SupervisorFixture>,
        req: axum::extract::Request,
    ) -> axum::response::Response {
        if req.method() != axum::http::Method::POST {
            if req.method() == axum::http::Method::GET {
                fx.get_count.fetch_add(1, Ordering::SeqCst);
            }
            return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
        }
        if !fx.accepting.load(Ordering::SeqCst) {
            return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
        }
        let session_header = req
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body_bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let value: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
        let id = value["id"].as_u64().unwrap_or(0);
        if value["method"] == "server/discover" {
            fx.discover_count.fetch_add(1, Ordering::SeqCst);
            if fx.hold_discover.swap(false, Ordering::SeqCst) {
                fx.started.notify_one();
                fx.release.notified().await;
            }
            if fx.discover_2026.load(Ordering::SeqCst) {
                return Json(json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "protocolVersion": "2026-07-28",
                        "capabilities": {"tools": {"listChanged": true}},
                        "serverInfo": {"name": "fake-http-2026", "version": "0.0.0"},
                        "tools": []
                    },
                    "id": id,
                }))
                .into_response();
            }
            // Legacy upstream: the probe is unknown.
            return Json(json!({
                "jsonrpc": "2.0",
                "error": {"code": -32601, "message": "method not found"},
                "id": id,
            }))
            .into_response();
        }
        if value["method"] == "initialize" {
            let n = fx.init_count.fetch_add(1, Ordering::SeqCst) + 1;
            let held_failure = *fx.fail_init.lock().unwrap();
            if let Some(failure) = held_failure {
                fx.started.notify_one();
                fx.release.notified().await;
                let mut resp = match failure {
                    HeldInitFailure::Http503 => {
                        return (StatusCode::SERVICE_UNAVAILABLE, "delayed handshake failure")
                            .into_response();
                    }
                    HeldInitFailure::JsonRpcError => Json(json!({
                        "jsonrpc": "2.0",
                        "error": {"code": -32000, "message": "initialize rejected"},
                        "id": id,
                    }))
                    .into_response(),
                    HeldInitFailure::MalformedBody => (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        "this is not json-rpc",
                    )
                        .into_response(),
                };
                // A header the adapter must NOT adopt: `current_session` is
                // left alone, so the previous id stays the only valid one.
                resp.headers_mut().insert(
                    MCP_SESSION_ID_HEADER.clone(),
                    reqwest::header::HeaderValue::from_str(&format!("rejected-sess-{}", n))
                        .unwrap(),
                );
                return resp;
            }
            let sid = fx
                .issue_session
                .load(Ordering::SeqCst)
                .then(|| format!("sess-{}", n));
            let on_arrival = fx.session_on_arrival.load(Ordering::SeqCst);
            if on_arrival {
                *fx.current_session.lock().unwrap() = sid.clone();
            }
            if fx.hold_init.swap(false, Ordering::SeqCst) {
                fx.started.notify_one();
                fx.release.notified().await;
            }
            let delay = fx.init_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if !on_arrival {
                *fx.current_session.lock().unwrap() = sid.clone();
            }
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
            if let Some(sid) = sid {
                resp.headers_mut().insert(
                    MCP_SESSION_ID_HEADER.clone(),
                    reqwest::header::HeaderValue::from_str(&sid).unwrap(),
                );
            }
            return resp;
        }
        let expected = fx.current_session.lock().unwrap().clone();
        if session_header != expected {
            return (StatusCode::BAD_REQUEST, "stale or missing session").into_response();
        }
        if value.get("id").is_none() {
            return (StatusCode::ACCEPTED, "").into_response();
        }
        if value["method"] == "tools/list" {
            fx.tools_list_count.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "jsonrpc": "2.0",
                "result": {"tools": [
                    {"name": "ping", "description": "p", "inputSchema": {"type": "object"}}
                ]},
                "id": id,
            }))
            .into_response();
        }
        Json(json!({"jsonrpc": "2.0", "result": {"ok": true}, "id": id})).into_response()
    }

    /// Swap in a fast backoff schedule so the supervisor tests run in
    /// milliseconds instead of the production 1 s base.
    async fn use_fast_backoff(adapter: &HttpAdapter) {
        *adapter.crash_tracker.lock().await = CrashTracker::new_test(
            Duration::from_millis(20),
            usize::MAX,
            Duration::from_secs(60),
        );
    }

    /// Drive the reactive-health demotion exactly as repeated transport-dead
    /// requests would, without needing a second (dead) upstream URL.
    async fn demote_via_transport_failures(adapter: &HttpAdapter) {
        for _ in 0..TRANSPORT_FAILURE_THRESHOLD {
            adapter.note_transport_failure().await;
        }
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "threshold crossing must demote the adapter"
        );
    }

    async fn wait_until<F: Fn() -> bool>(deadline: Duration, cond: F) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    /// The core regression: once the adapter demotes itself to `Unhealthy`, it
    /// must return to `Healthy` on its own — no caller ever issues another
    /// request. Before the supervisor existed, recovery depended entirely on
    /// the next inbound request happening to succeed.
    #[tokio::test]
    async fn supervisor_recovers_health_without_caller_traffic() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        demote_via_transport_failures(&adapter).await;

        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(
            recovered,
            "supervisor must restore Healthy with no caller traffic"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "recovery must come from a fresh handshake, not a plain request"
        );
        assert_eq!(
            adapter.transport_failures.load(Ordering::SeqCst),
            0,
            "a successful reconnect resets the failure counter"
        );
        assert!(
            rx.try_recv().is_ok(),
            "a successful reconnect must tick tools_changed so the catalog is re-fetched"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// While the upstream keeps refusing the handshake the supervisor must
    /// keep retrying with escalating backoff (1×, 2×, 4× … the base) rather
    /// than hammering at a fixed cadence — and must still recover once the
    /// upstream comes back.
    #[tokio::test]
    async fn supervisor_backoff_escalates_across_failed_attempts() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;

        // Take the upstream down, then demote so the supervisor starts.
        fx.accepting.store(false, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;

        // 503 on initialize doesn't count as an issued session; count the
        // attempts via the tracker instead, which increments once per try.
        let escalated = wait_until(Duration::from_secs(5), || {
            adapter
                .crash_tracker
                .try_lock()
                .map(|t| t.consecutive_failures >= 3)
                .unwrap_or(false)
        })
        .await;
        assert!(escalated, "supervisor should have made several attempts");
        {
            let tracker = adapter.crash_tracker.lock().await;
            assert!(
                tracker.backoff_duration() >= Duration::from_millis(80),
                "after 3+ consecutive failures the backoff must be ≥ 4× the base, got {:?}",
                tracker.backoff_duration()
            );
        }
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "adapter must stay Unhealthy while the upstream is down"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        // Bring the upstream back: the next attempt succeeds and backoff resets.
        fx.accepting.store(true, Ordering::SeqCst);
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(
            recovered,
            "supervisor must recover once the upstream returns"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            adapter.crash_tracker.lock().await.consecutive_failures,
            0,
            "a successful reconnect must reset the backoff"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// `shutdown()` must stop a supervisor that is mid-backoff: the task
    /// finishes, no further handshake attempts reach the upstream, and health
    /// ends `Stopped` (not flipped back by a late reconnect).
    #[tokio::test]
    async fn supervisor_exits_on_shutdown() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        // Long backoff so the supervisor is guaranteed to be sleeping when
        // shutdown arrives.
        *adapter.crash_tracker.lock().await =
            CrashTracker::new_test(Duration::from_secs(30), usize::MAX, Duration::from_secs(60));

        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "initialize must arm the supervisor"
        );

        fx.accepting.store(false, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        // Let the supervisor observe the notification and enter its backoff.
        tokio::time::sleep(Duration::from_millis(50)).await;

        adapter.shutdown().await.expect("shutdown succeeds");
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert!(
            lock_slot(&adapter.reconnect_handle).is_none(),
            "shutdown must take and join the supervisor handle"
        );

        // No late reconnect: bring the upstream back and confirm nothing
        // re-handshakes or flips health.
        fx.accepting.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.health(), HealthStatus::Stopped);

        server.abort();
    }

    /// The management restart fallback calls `shutdown()` and then
    /// `initialize()` on the same adapter. That second initialize must be a
    /// full restart: a fresh GET listener (or push invalidations are lost
    /// for the rest of the process) and a re-armed supervisor that still
    /// recovers a later demotion — not merely a `Healthy` health flip.
    #[tokio::test]
    async fn reinitialize_after_shutdown_restores_listener_and_recovery() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        assert!(
            wait_until(Duration::from_secs(2), || {
                fx.get_count.load(Ordering::SeqCst) == 1
            })
            .await,
            "first initialize spawns the GET listener"
        );

        adapter.shutdown().await.expect("shutdown succeeds");
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert!(lock_slot(&adapter.reconnect_handle).is_none());
        assert!(lock_slot(&adapter.listener_handle).is_none());

        adapter.initialize().await.expect("re-initialize succeeds");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);
        assert!(
            wait_until(Duration::from_secs(2), || {
                fx.get_count.load(Ordering::SeqCst) == 2
            })
            .await,
            "re-initialize after shutdown must spawn a replacement GET listener"
        );
        assert!(
            lock_slot(&adapter.listener_handle).is_some(),
            "re-initialize must install the new listener"
        );
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "re-initialize must re-arm the supervisor"
        );

        // The re-armed lifecycle must still self-heal.
        use_fast_backoff(&adapter).await;
        demote_via_transport_failures(&adapter).await;
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
            })
            .await,
            "supervisor re-armed by re-initialize must recover a later demotion"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            3,
            "recovery must come from a supervisor handshake"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Startup against a dead upstream (connection refused): `initialize()`
    /// fails and leaves the adapter `Unhealthy(<reason>)` with NO supervisor.
    /// `retry_initialize_in_background` must arm one so that, once the
    /// upstream comes up at the same address, the adapter handshakes and
    /// flips `Healthy` on its own — no caller traffic, and one
    /// `tools_changed` tick so the registry re-fetches the catalog.
    #[tokio::test]
    async fn retry_in_background_recovers_from_dead_upstream_at_startup() {
        let (addr, url) = reserve_dead_upstream().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        let err = adapter
            .initialize()
            .await
            .expect_err("initialize against a dead upstream fails");
        match adapter.health() {
            HealthStatus::Unhealthy(reason) => {
                assert_eq!(reason, err.to_string(), "health carries the init error")
            }
            other => panic!(
                "expected Unhealthy after failed initialize, got {:?}",
                other
            ),
        }
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "a failed initialize arms the supervisor itself"
        );
        assert_eq!(
            adapter.stderr_lines().await,
            vec![format!("[ERROR] {}", err)],
            "logs view surfaces the init error like a FailedAdapter did"
        );

        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");
        // Idempotent: a second arm (the registration path used to call this)
        // neither duplicates the supervisor nor disturbs it.
        adapter.retry_initialize_in_background().await;
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "retry_initialize_in_background keeps the supervisor armed"
        );

        // Still down: the supervisor keeps retrying and the adapter stays
        // Unhealthy (with a meaningful reason) the whole time.
        tokio::time::sleep(Duration::from_millis(120)).await;
        match adapter.health() {
            HealthStatus::Unhealthy(reason) => assert!(!reason.is_empty()),
            other => panic!("expected Unhealthy while upstream is down, got {:?}", other),
        }
        assert!(rx.try_recv().is_err(), "no tick while still down");

        // Upstream comes up at the configured address.
        let listener = TcpListener::bind(addr)
            .await
            .expect("rebind the reserved upstream port");
        let (_url, fx, server) = start_supervisor_fixture_on(listener).await;

        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
            })
            .await,
            "supervisor must bring a startup-dead upstream to Healthy with no caller traffic"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            1,
            "exactly one handshake reaches the upstream once it is up"
        );
        assert!(
            rx.try_recv().is_ok(),
            "the recovery must tick tools_changed so the catalog is re-fetched"
        );
        assert!(
            adapter.stderr_lines().await.is_empty(),
            "no stale [ERROR] line once healthy"
        );
        assert_eq!(adapter.transport_failures.load(Ordering::SeqCst), 0);

        // The armed lifecycle is the standard one: a later demotion still
        // self-heals via the same supervisor.
        demote_via_transport_failures(&adapter).await;
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
            })
            .await,
            "supervisor armed at startup must recover a later demotion too"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Same recovery when the upstream is reachable but failing the
    /// handshake (503 on `initialize`) at startup — the alive-but-erroring
    /// case the reactive path never demotes on.
    #[tokio::test]
    async fn retry_in_background_recovers_from_failing_handshake_at_startup() {
        let (url, fx, server) = start_supervisor_fixture().await;
        fx.accepting.store(false, Ordering::SeqCst);
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect_err("503 handshake fails initialize");
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 0);

        use_fast_backoff(&adapter).await;
        adapter.retry_initialize_in_background().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "still Unhealthy while the upstream keeps failing the handshake"
        );

        fx.accepting.store(true, Ordering::SeqCst);
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
            })
            .await,
            "supervisor must recover once the handshake succeeds"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// `retry_initialize_in_background` follows the same rule as the
    /// reactive demotion: never spawn a supervisor for a `Stopped` adapter
    /// (or one that is not `Unhealthy`), so a shutdown adapter stays down.
    #[tokio::test]
    async fn retry_in_background_is_noop_unless_unhealthy() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        adapter.shutdown().await.expect("shutdown succeeds");
        assert_eq!(adapter.health(), HealthStatus::Stopped);

        use_fast_backoff(&adapter).await;
        adapter.retry_initialize_in_background().await;
        assert!(
            lock_slot(&adapter.reconnect_handle).is_none(),
            "no supervisor for a Stopped adapter"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        server.abort();
    }

    /// While `Unhealthy` and never initialized, `list_tools` must answer
    /// `Ok([])` like a `FailedAdapter` — no upstream `tools/list`, and no
    /// false `Healthy` from a stray successful request — so a catalog rebuild
    /// neither pays connect latency nor "recovers" an adapter that has no
    /// session. Only the supervisor's handshake may bring it back.
    #[tokio::test]
    async fn never_initialized_list_tools_is_empty_without_upstream_traffic() {
        let (url, fx, server) = start_supervisor_fixture().await;
        fx.accepting.store(false, Ordering::SeqCst);
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect_err("503 handshake fails initialize");
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 0);
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        // Worst case: the upstream is now alive and would happily answer
        // `tools/list` — the never-initialized adapter must still not ask.
        fx.accepting.store(true, Ordering::SeqCst);
        let tools = adapter
            .list_tools()
            .await
            .expect("never-initialized list_tools is Ok");
        assert!(tools.is_empty(), "no tools before a handshake: {tools:?}");
        assert_eq!(
            fx.tools_list_count.load(Ordering::SeqCst),
            0,
            "no upstream tools/list while never initialized"
        );
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "catalog read must not flip health without a handshake"
        );
        // A successful request on another path must not fake a recovery
        // either: there is still no session.
        let _ = adapter.call_tool("ping", json!({})).await;
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "note_request_success must not flip Healthy before a handshake"
        );
        assert!(
            rx.try_recv().is_err(),
            "no tools_changed without a recovery"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 0);

        // The supervisor's handshake is the only way back.
        use_fast_backoff(&adapter).await;
        adapter.retry_initialize_in_background().await;
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
            })
            .await,
            "supervisor recovers once the handshake succeeds"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_ok(), "recovery ticks tools_changed");
        let tools = adapter.list_tools().await.expect("tools after handshake");
        assert_eq!(tools.len(), 1);
        assert_eq!(fx.tools_list_count.load(Ordering::SeqCst), 1);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Same contract when the upstream is not even listening: `Ok([])`
    /// immediately, without a connect attempt.
    #[tokio::test]
    async fn never_initialized_list_tools_is_empty_when_upstream_dead() {
        let (_addr, url) = reserve_dead_upstream().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect_err("connection refused fails initialize");
        let tools = adapter
            .list_tools()
            .await
            .expect("never-initialized list_tools is Ok");
        assert!(tools.is_empty());
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        assert_eq!(
            adapter.transport_failures.load(Ordering::SeqCst),
            0,
            "no request was issued, so nothing was counted"
        );
        adapter.shutdown().await.unwrap();
    }

    /// Non-regression: after the FIRST successful handshake the guard is
    /// latched, so an adapter demoted to `Unhealthy` later keeps the
    /// pre-existing behaviour — `list_tools` reaches the upstream and a
    /// success recovers health reactively (with its `tools_changed` tick).
    #[tokio::test]
    async fn list_tools_after_first_handshake_still_recovers_reactively() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        demote_via_transport_failures(&adapter).await;

        let tools = adapter.list_tools().await.expect("upstream is reachable");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            fx.tools_list_count.load(Ordering::SeqCst),
            1,
            "list_tools must still reach the upstream once a session existed"
        );
        assert_eq!(
            adapter.health(),
            HealthStatus::Healthy,
            "a successful request recovers health reactively"
        );
        assert!(
            rx.try_recv().is_ok(),
            "reactive recovery ticks tools_changed"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// A reconnect re-runs the full handshake, so the upstream may hand out a
    /// new `Mcp-Session-Id`. The adapter must adopt it: subsequent requests
    /// carry the fresh id (the fixture 400s anything still echoing the old
    /// one), and the GET listener is respawned rather than left attached to
    /// the dead session.
    #[tokio::test]
    async fn supervisor_reconnect_refreshes_session_id() {
        let (url, fx, server) = start_supervisor_fixture().await;
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 0);
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-1"),
            "initial handshake captures the first session id"
        );
        let first_listener_id = lock_slot(&adapter.listener_handle)
            .as_ref()
            .map(|h| h.id())
            .expect("listener spawned by initialize");

        demote_via_transport_failures(&adapter).await;
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(recovered);

        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-2"),
            "reconnect must adopt the upstream's new session id"
        );
        let second_listener_id = lock_slot(&adapter.listener_handle)
            .as_ref()
            .map(|h| h.id())
            .expect("listener respawned by reconnect");
        assert_ne!(
            first_listener_id, second_listener_id,
            "reconnect must respawn the GET listener for the new session"
        );

        // A request after reconnect must succeed — i.e. it echoes `sess-2`.
        let result = adapter
            .call_tool("x", json!({}))
            .await
            .expect("post-reconnect request carries the refreshed session id");
        assert_eq!(result["ok"], true);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    fn drain_ticks(rx: &mut broadcast::Receiver<()>) -> usize {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    /// A caller's request that succeeds while the supervisor is sleeping out
    /// its backoff recovers the adapter reactively (and ticks). The supervisor
    /// must then stand down: no redundant handshake, no second tick.
    #[tokio::test]
    async fn reactive_recovery_during_backoff_emits_only_one_tick() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        // Backoff long enough that the caller's request lands mid-sleep.
        *adapter.crash_tracker.lock().await = CrashTracker::new_test(
            Duration::from_millis(300),
            usize::MAX,
            Duration::from_secs(60),
        );
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        demote_via_transport_failures(&adapter).await;
        // The supervisor records the attempt right before it sleeps, and its
        // `recovered` future is registered before that, so once the counter
        // reads 1 the attempt is in (or entering) its backoff and a recovery
        // signal fired now is guaranteed to reach it.
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter
                    .crash_tracker
                    .try_lock()
                    .map(|t| t.consecutive_failures == 1)
                    .unwrap_or(false)
            })
            .await,
            "supervisor should have entered its backoff"
        );
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));

        // Reactive recovery: a plain request succeeds against the live upstream.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live upstream answers");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(drain_ticks(&mut rx), 1, "reactive recovery ticks once");

        // Standing down resets the backoff — that reset is the observable
        // end of the supervisor's reaction, so wait for it rather than
        // outliving the sleep by a wall-clock margin.
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter
                    .crash_tracker
                    .try_lock()
                    .map(|t| t.consecutive_failures == 0)
                    .unwrap_or(false)
            })
            .await,
            "supervisor must stand down (backoff reset) after a reactive recovery"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            1,
            "no redundant handshake after a reactive recovery"
        );
        assert_eq!(drain_ticks(&mut rx), 0, "no second tick");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "supervisor keeps waiting for the next demotion"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Same race, later window: the caller's request succeeds while the
    /// supervisor's handshake is already in flight. The handshake completes
    /// (adopting the upstream's newest session) but the tick is owned by the
    /// path that performed the actual `Unhealthy → Healthy` flip, so exactly
    /// one tick is observed and the adapter ends up on a working session.
    #[tokio::test]
    async fn reactive_recovery_during_inflight_handshake_emits_only_one_tick() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        // Park the reconnect's `initialize` at the upstream so the race is
        // ordered by barriers, not timing.
        fx.hold_init.store(true, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), fx.started.notified())
            .await
            .expect("supervisor handshake should have started");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));

        // The upstream still honours `sess-1` until the held initialize
        // completes, so a caller's request succeeds and recovers reactively.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live upstream answers on the old session");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(drain_ticks(&mut rx), 1, "reactive recovery ticks once");

        // Let the in-flight handshake finish. Its last step resets the
        // backoff tracker (still at 1 from the attempt), which is the
        // completion signal to wait for.
        fx.release.notify_one();
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter
                    .crash_tracker
                    .try_lock()
                    .map(|t| t.consecutive_failures == 0)
                    .unwrap_or(false)
            })
            .await,
            "the in-flight handshake should run to completion"
        );
        assert_eq!(drain_ticks(&mut rx), 0, "handshake must not tick again");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-2"),
            "handshake adopts the upstream's newest session"
        );
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("post-handshake request carries the current session id");

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 4, Copilot): a NEW demotion lands while the
    /// supervisor's `initialize` is already on the wire (the adapter had
    /// recovered reactively in between, so the attempt is obsolete but must
    /// run to completion). There is no generation to compare: the handshake
    /// that then succeeds is the newest evidence about the upstream, and it
    /// promotes to `Healthy` on the session the upstream just issued. The
    /// newer demotion's reconnect permit is not lost — it is consumed by a
    /// supervisor pass that finds the adapter `Healthy` and stands down — and
    /// because health is `Healthy` again the next threshold crossing demotes
    /// and re-arms as usual, so no outage can leave the adapter `Unhealthy`
    /// with nothing retrying. Pins that contract deterministically.
    #[tokio::test]
    async fn demotion_during_inflight_handshake_promotes_and_keeps_retrying() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        fx.hold_init.store(true, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), fx.started.notified())
            .await
            .expect("supervisor handshake should have started");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);

        // Reactive recovery on the still-honoured `sess-1` makes the parked
        // attempt obsolete ...
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live upstream answers on the old session");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(drain_ticks(&mut rx), 1, "reactive recovery ticks once");

        // ... and a NEWER outage demotes again while that attempt is still
        // parked at the upstream. Its permit is stored on `reconnect_notify`.
        demote_via_transport_failures(&adapter).await;
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "the supervisor is inside the parked attempt; no second handshake starts"
        );

        // The parked handshake completes: it promotes on the session the
        // upstream just issued (one flip, one tick) and resets the backoff.
        fx.release.notify_one();
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
                    && adapter
                        .crash_tracker
                        .try_lock()
                        .map(|t| t.consecutive_failures == 0)
                        .unwrap_or(false)
            })
            .await,
            "the in-flight handshake promotes to Healthy"
        );
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-2"),
            "the promoted state is the session the upstream actually issued"
        );
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("post-handshake request carries the current session id");
        assert_eq!(adapter.transport_failures.load(Ordering::SeqCst), 0);
        assert_eq!(
            drain_ticks(&mut rx),
            1,
            "the handshake's flip ticks exactly once"
        );

        // The stored permit only makes the supervisor look, see `Healthy`,
        // and go back to waiting — it never handshakes on a Healthy adapter.
        assert!(
            wait_until(Duration::from_secs(5), || {
                lock_slot(&adapter.reconnect_handle)
                    .as_ref()
                    .is_some_and(|h| !h.is_finished())
            })
            .await,
            "supervisor keeps running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "no redundant handshake after the stale permit"
        );

        // A later outage is not silently swallowed: the threshold crossing
        // demotes (health was Healthy) and re-arms the supervisor, which
        // recovers on its own.
        demote_via_transport_failures(&adapter).await;
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
                    && fx.init_count.load(Ordering::SeqCst) == 3
            })
            .await,
            "the next demotion still drives a supervisor recovery"
        );
        assert_eq!(drain_ticks(&mut rx), 1, "that recovery ticks once");
        assert_eq!(adapter.session_id.read().await.as_deref(), Some("sess-3"));

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 4, Copilot): a caller-owned `initialize()` on an
    /// already-enabled endpoint (the management enable path re-initializes
    /// regardless of `was_disabled`) while the supervisor's reconnect
    /// handshake is in flight on its `task_clone`. Without serialization the
    /// two `initialize` exchanges overlap and, when their responses complete
    /// out of order, the OLDER response commits last: the adapter keeps a
    /// session the upstream has already replaced and every later request
    /// gets 400. With `handshake_lock` the caller waits for the in-flight
    /// handshake, then runs its own; the committed session is the one from
    /// the last-completed handshake and both paths end `Healthy`.
    /// Deterministic: the fixture parks the supervisor's `initialize`, and
    /// issues sessions on arrival so an overlap would be observable.
    #[tokio::test]
    async fn caller_initialize_waits_for_inflight_supervisor_handshake() {
        let (url, fx, server) = start_supervisor_fixture().await;
        fx.session_on_arrival.store(true, Ordering::SeqCst);
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        // Shares every piece of state with `adapter`; lets the test observe
        // while `adapter` is mutably borrowed by its own `initialize()`.
        let observer = adapter.task_clone();
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");

        // The supervisor's reconnect handshake is parked at the upstream
        // (its `sess-2` is already issued and honoured).
        fx.hold_init.store(true, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), fx.started.notified())
            .await
            .expect("supervisor handshake should have started");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);

        // A caller-owned initialize arrives while that handshake is in
        // flight. It must NOT put a second `initialize` on the wire.
        {
            let mut init = adapter.initialize();
            assert!(
                tokio::time::timeout(Duration::from_millis(300), &mut init)
                    .await
                    .is_err(),
                "caller initialize waits for the in-flight supervisor handshake"
            );
            assert_eq!(
                fx.init_count.load(Ordering::SeqCst),
                2,
                "no overlapping initialize while the supervisor's is in flight"
            );
            assert!(
                matches!(observer.health(), HealthStatus::Unhealthy(_)),
                "the waiting caller has not touched health yet"
            );

            // The parked handshake completes (`sess-2`, one flip), then the
            // caller's own handshake runs and commits `sess-3` — the session
            // the upstream now honours.
            fx.release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), &mut init)
                .await
                .expect("caller initialize completes once the lock is released")
                .expect("caller initialize succeeds");
        }
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 3);
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-3"),
            "the committed session is the last-completed handshake's"
        );
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(observer.health(), HealthStatus::Healthy);
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("requests carry the session the upstream honours");
        assert_eq!(
            drain_ticks(&mut rx),
            1,
            "the supervisor's flip ticks once; the caller's handshake found no Unhealthy to flip"
        );

        // The lock is released and the supervisor still runs: a later outage
        // is recovered as usual.
        demote_via_transport_failures(&adapter).await;
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
                    && fx.init_count.load(Ordering::SeqCst) == 4
            })
            .await,
            "the next demotion still drives a supervisor recovery"
        );
        assert_eq!(adapter.session_id.read().await.as_deref(), Some("sess-4"));
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("post-recovery request carries the current session");

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// An upstream that restarted and came back sessionless omits
    /// `Mcp-Session-Id` from `initialize`. The reconnect must DISCARD the
    /// previous id rather than keep echoing it (which the fixture, like a real
    /// upstream, rejects with 400).
    #[tokio::test]
    async fn reconnect_without_session_header_discards_old_session() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        assert_eq!(adapter.session_id.read().await.as_deref(), Some("sess-1"));

        fx.issue_session.store(false, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(
            recovered,
            "supervisor must recover against the sessionless upstream"
        );
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            None,
            "a sessionless initialize must clear the stale session id"
        );

        let result = adapter
            .call_tool("x", json!({}))
            .await
            .expect("post-reconnect request must not carry the old session id");
        assert_eq!(result["ok"], true);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// `shutdown()` while the supervisor's handshake is in flight: the
    /// supervisor is stopped BEFORE the listener slot is drained, so the
    /// interrupted handshake can neither leave a live listener behind nor
    /// flip health back after shutdown.
    #[tokio::test]
    async fn shutdown_during_inflight_handshake_leaves_no_listener() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;

        fx.init_delay_ms.store(400, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        let in_flight = wait_until(Duration::from_secs(5), || {
            fx.init_count.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(in_flight, "supervisor handshake should have started");

        adapter.shutdown().await.expect("shutdown succeeds");
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert!(lock_slot(&adapter.reconnect_handle).is_none());
        assert!(lock_slot(&adapter.listener_handle).is_none());

        // Outlive the delayed initialize: nothing may be installed or flipped.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            lock_slot(&adapter.listener_handle).is_none(),
            "an interrupted handshake must not install a listener after shutdown"
        );
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "no further handshake attempts after shutdown"
        );

        server.abort();
    }

    /// Belt and braces for the same race when the handshake cannot be
    /// joined (e.g. `Drop`): once shutdown has been signalled,
    /// `spawn_get_listener` refuses to repopulate the slot.
    #[tokio::test]
    async fn spawn_get_listener_is_a_no_op_after_shutdown_signalled() {
        let (url, _fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        adapter.shutdown().await.expect("shutdown succeeds");
        assert!(lock_slot(&adapter.listener_handle).is_none());

        adapter.spawn_get_listener().await;
        assert!(
            lock_slot(&adapter.listener_handle).is_none(),
            "no listener may be installed once shutdown has begun"
        );

        server.abort();
    }

    /// The concurrent-installation window: `spawn_get_listener` has taken the
    /// slot lock and passed the `shutting_down` check when the caller-owned
    /// adapter is dropped (the OAuth wrapper drops superseded inner adapters
    /// without `shutdown()`). `Drop` must still drain the listener that gets
    /// installed — it blocks on the slot instead of giving up on a failed
    /// `try_lock`. The test plays the critical section itself: it holds the
    /// slot, starts the drop on another thread, waits for the flag, then
    /// installs a listener and releases the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // Holding the slot across the wait IS the scenario under test.
    #[allow(clippy::await_holding_lock)]
    async fn drop_during_listener_installation_aborts_new_listener() {
        let adapter = HttpAdapter::new(HttpConfig::new("http://127.0.0.1:1/mcp"));
        let slot = adapter.listener_handle.clone();
        let flag = adapter.shutting_down.clone();

        let mut guard = lock_slot(&slot);
        let dropper = std::thread::spawn(move || drop(adapter));
        assert!(
            wait_until(Duration::from_secs(5), || flag.load(Ordering::SeqCst)).await,
            "Drop must signal shutdown before it tries the slot"
        );
        assert!(
            !dropper.is_finished(),
            "Drop must wait for the in-progress installation, not give up"
        );

        let late_listener = tokio::spawn(std::future::pending::<()>());
        let late_abort = late_listener.abort_handle();
        *guard = Some(late_listener);
        drop(guard);

        tokio::task::spawn_blocking(move || dropper.join().unwrap())
            .await
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || late_abort.is_finished()).await,
            "Drop must abort a listener installed during the shutdown window"
        );
        assert!(lock_slot(&slot).is_none(), "Drop must drain the slot");
    }

    /// Verifier probe: a reactive recovery while the supervisor's attempt is
    /// still in the (stateless) dialect probe must retire the attempt — no
    /// second `initialize` is ever sent.
    #[tokio::test]
    async fn obsolete_discovery_does_not_start_another_initialize() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().unwrap();
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        fx.hold_discover.store(true, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), fx.started.notified())
            .await
            .expect("supervisor attempt should reach the dialect probe");

        // Caller traffic recovers the adapter while the probe is held.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server answers");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        rx.try_recv().expect("one reactive recovery tick");

        fx.release.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            1,
            "an attempt obsoleted during discovery must not send initialize"
        );
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(rx.try_recv().is_err(), "exactly one tick per recovery");
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "supervisor keeps waiting for the next demotion"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Verifier probe: an `initialize` the supervisor already sent fails
    /// (delayed 503) AFTER caller traffic recovered the adapter. The stale
    /// failure must not write `Unhealthy` over the recovered `Healthy`.
    #[tokio::test]
    async fn obsolete_failed_handshake_does_not_undo_reactive_recovery() {
        obsolete_failed_handshake_keeps_recovery(HeldInitFailure::Http503).await;
    }

    /// PR #163 review (round 4): the obsolete `initialize` fails at the BODY
    /// stage — HTTP 200 carrying a `Mcp-Session-Id` header but a JSON-RPC
    /// error object. The header must not be adopted: the upstream never
    /// created that session, so committing it would leave a `Healthy`
    /// adapter whose every request gets 400, with no supervisor to fix it.
    #[tokio::test]
    async fn obsolete_handshake_with_jsonrpc_error_body_keeps_working_session() {
        obsolete_failed_handshake_keeps_recovery(HeldInitFailure::JsonRpcError).await;
    }

    /// Same as above with a body that is not JSON-RPC at all.
    #[tokio::test]
    async fn obsolete_handshake_with_malformed_body_keeps_working_session() {
        obsolete_failed_handshake_keeps_recovery(HeldInitFailure::MalformedBody).await;
    }

    async fn obsolete_failed_handshake_keeps_recovery(failure: HeldInitFailure) {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().unwrap();

        fx.fail_init_with(Some(failure));
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), fx.started.notified())
            .await
            .expect("supervisor attempt should reach initialize");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 2);

        // Caller traffic recovers the adapter while initialize is held.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server answers");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        rx.try_recv().expect("one reactive recovery tick");

        // Let the held initialize fail. The supervisor then finds the adapter
        // Healthy and stands down, which resets the backoff tracker (at 1
        // from the attempt) — wait for that rather than a wall-clock margin.
        fx.fail_init_with(None);
        fx.release.notify_one();
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter
                    .crash_tracker
                    .try_lock()
                    .map(|t| t.consecutive_failures == 0)
                    .unwrap_or(false)
            })
            .await,
            "the supervisor should process the failed attempt and stand down"
        );
        assert_eq!(
            adapter.health(),
            HealthStatus::Healthy,
            "an obsolete reconnect failure must not undo recovery"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "the supervisor must stand down rather than retry"
        );
        assert!(rx.try_recv().is_err(), "exactly one tick per recovery");
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-1"),
            "a failed initialize never replaces the working session"
        );
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("the original session is still valid");

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 4): `recovered_notify` is published AFTER the
    /// `health` lock is released, so a signal from an earlier flip can reach
    /// the supervisor once a LATER demotion has already restarted it. Such a
    /// stale signal must not stand the supervisor down while the adapter is
    /// `Unhealthy` — nothing would ever restart it. Fired here directly while
    /// the attempt is parked in the (stateless) dialect probe, which is the
    /// deterministic equivalent of the race. Round 5: the stale hint must
    /// also leave the in-flight probe alone rather than cancel it and start
    /// a new attempt — the parked probe is released afterwards and is the
    /// only one the upstream ever sees.
    #[tokio::test]
    async fn stale_recovery_signal_during_dialect_probe_keeps_retrying() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().unwrap();

        fx.hold_discover.store(true, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), fx.started.notified())
            .await
            .expect("supervisor attempt should reach the dialect probe");
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        // One probe from the caller's initial handshake, one from this attempt.
        assert_eq!(fx.discover_count.load(Ordering::SeqCst), 2);

        // The stale signal: nothing recovered the adapter.
        adapter.recovered_notify.notify_waiters();

        // The probe is still parked at the upstream; the attempt waits on
        // it instead of abandoning it for a fresh one.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        assert_eq!(
            fx.discover_count.load(Ordering::SeqCst),
            2,
            "a stale hint must not cancel and re-issue the in-flight dialect probe"
        );
        assert_eq!(
            adapter.crash_tracker.lock().await.consecutive_failures,
            1,
            "a stale hint must not record another attempt"
        );

        fx.release.notify_one();
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(
            recovered,
            "a stale recovery signal must not stand the supervisor down while Unhealthy"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "recovery must come from the supervisor's retried handshake"
        );
        assert_eq!(fx.discover_count.load(Ordering::SeqCst), 2);
        assert_eq!(drain_ticks(&mut rx), 1, "exactly one tick per recovery");
        assert_eq!(adapter.crash_tracker.lock().await.consecutive_failures, 0);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 6, Copilot): the supervisor must not carry a
    /// dialect-probe result across its `handshake_lock` wait. A 2026 result
    /// selects the stateless path, which sends nothing fresh to the upstream
    /// in `complete_handshake`: if the upstream went away (or another
    /// handshake re-demoted the adapter) while the attempt waited for the
    /// lock, the stale probe would still commit `Healthy` and consume the
    /// reconnect permit. The probe now runs under the lock, so it observes
    /// the upstream as it is when the attempt actually proceeds.
    #[tokio::test]
    async fn dialect_probe_runs_under_handshake_lock() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initial handshake");
        use_fast_backoff(&adapter).await;
        assert_eq!(fx.discover_count.load(Ordering::SeqCst), 1);

        // From now on the upstream answers the probe as a 2026 server.
        fx.discover_2026.store(true, Ordering::SeqCst);
        // Hold the handshake lock exactly as a caller-owned `initialize()`
        // would, then demote: the attempt backs off and queues on the lock.
        let held = adapter.handshake_lock.lock().await;
        demote_via_transport_failures(&adapter).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));

        // The upstream dies while the attempt is waiting for the lock.
        fx.accepting.store(false, Ordering::SeqCst);
        drop(held);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "a probe result from before the handshake-lock wait must not be committed once the upstream is unreachable"
        );
        assert_eq!(
            *adapter.upstream_dialect.read().await,
            ProtocolVersion::V2025_03_26,
            "no stale 2026 dialect may be published either"
        );
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "supervisor keeps retrying against the dead upstream"
        );

        // The upstream returns: the attempt probes it fresh and recovers.
        fx.accepting.store(true, Ordering::SeqCst);
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(recovered, "supervisor recovers once the upstream is back");
        assert!(
            adapter.upstream_dialect.read().await.is_2026(),
            "the recovery used a probe taken under the lock"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 6, Copilot): the JIT credential lookup before
    /// the `initialize` POST is an await window in which caller traffic can
    /// recover the adapter. The attempt is still cancellable there (nothing
    /// has reached the upstream), so it must re-check health after the
    /// lookup instead of sending a redundant `initialize` that rotates the
    /// recovered session. Deterministic via the test-only gate at that point.
    #[tokio::test]
    async fn recovery_during_credential_loading_sends_no_initialize() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        let gate = Arc::new(TestGate {
            reached: Notify::new(),
            release: Notify::new(),
        });
        adapter.handshake_send_gate = Some(gate.clone());
        // Pre-arm one pass for the caller-owned handshake, then consume the
        // `reached` permit it leaves behind so the supervisor's is distinct.
        gate.release.notify_one();
        adapter.initialize().await.expect("initial handshake");
        gate.reached.notified().await;
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().unwrap();
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), gate.reached.notified())
            .await
            .expect("supervisor attempt should reach the initialize send point");

        // Caller traffic recovers the adapter while the attempt is parked
        // right after its credential lookup.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server answers");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        rx.try_recv().expect("one reactive recovery tick");

        gate.release.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            1,
            "an attempt obsoleted during credential loading must not send initialize"
        );
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-1"),
            "the recovered session must not be rotated"
        );
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(rx.try_recv().is_err(), "exactly one tick per recovery");
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "supervisor keeps waiting for the next demotion"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 6b, Copilot): on the 2026 path the supervisor
    /// must not commit identity, dialect, session, listener and health after
    /// caller traffic recovered the legacy session during one of the awaits
    /// between the probe and the commit. Otherwise the adapter stays
    /// `Healthy` on a legacy connection while formatting every request as
    /// sessionless 2026 traffic, with no retry armed. The check runs under
    /// the commit's own lock, so the gate sits at the last await before it.
    #[tokio::test]
    async fn recovery_before_2026_commit_keeps_legacy_session() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        let gate = TestGate::new();
        adapter.handshake_send_gate = Some(gate.clone());
        // Pre-arm one pass for the caller-owned (legacy) handshake, then
        // consume the `reached` permit it leaves behind.
        gate.release.notify_one();
        adapter.initialize().await.expect("initial handshake");
        gate.reached.notified().await;
        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().unwrap();
        assert_eq!(
            adapter.upstream_dialect().await,
            ProtocolVersion::V2025_03_26
        );
        assert_eq!(adapter.session_id.read().await.as_deref(), Some("sess-1"));
        let name_before = adapter.upstream_server_name.read().await.clone();

        // The upstream now answers the probe as a 2026 server while still
        // honouring the legacy session, so the supervisor's attempt takes
        // the 2026 path.
        fx.discover_2026.store(true, Ordering::SeqCst);
        demote_via_transport_failures(&adapter).await;
        tokio::time::timeout(Duration::from_secs(5), gate.reached.notified())
            .await
            .expect("supervisor attempt should reach the 2026 commit point");

        // Caller traffic recovers the legacy session while the attempt is
        // parked before its commit.
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("live server answers");
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        rx.try_recv().expect("one reactive recovery tick");

        gate.release.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            adapter.upstream_dialect().await,
            ProtocolVersion::V2025_03_26,
            "an attempt obsoleted before the 2026 commit must not switch the dialect"
        );
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-1"),
            "the recovered legacy session must not be cleared"
        );
        assert_eq!(
            *adapter.upstream_server_name.read().await,
            name_before,
            "the obsolete attempt's identity must not be committed"
        );
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(rx.try_recv().is_err(), "exactly one tick per recovery");
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "supervisor keeps waiting for the next demotion"
        );

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 6, Copilot): `shutdown()` tears the session
    /// listener down, so it must also clear `handshake_completed`. Otherwise
    /// a re-`initialize()` that fails (upstream down) leaves the supervisor
    /// retrying while a stray success on the OLD session — which the
    /// upstream may still honour — would promote the adapter to `Healthy`
    /// through `note_request_success` and stop the retries, with no listener
    /// to carry server-initiated tool changes until another restart.
    #[tokio::test]
    async fn stale_success_after_shutdown_does_not_promote_without_handshake() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initial handshake");
        adapter.shutdown().await.unwrap();
        use_fast_backoff(&adapter).await;

        fx.accepting.store(false, Ordering::SeqCst);
        adapter
            .initialize()
            .await
            .expect_err("re-initialize fails while the upstream is down");
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        let gets_before = fx.get_count.load(Ordering::SeqCst);

        // A request on the old session succeeds during the supervisor's
        // backoff (the reactive path such a success drives).
        adapter.note_request_success().await;
        assert!(
            matches!(adapter.health(), HealthStatus::Unhealthy(_)),
            "a request success after shutdown must not promote an adapter whose handshake never respawned the listener"
        );
        assert!(
            !adapter.handshake_completed.load(Ordering::SeqCst),
            "shutdown returns the adapter to the never-initialized contract"
        );

        // Recovery still comes from a real handshake, which respawns the
        // GET listener.
        fx.accepting.store(true, Ordering::SeqCst);
        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        assert!(recovered, "supervisor recovers once the upstream is back");
        let listener_respawned = wait_until(Duration::from_secs(5), || {
            fx.get_count.load(Ordering::SeqCst) > gets_before
        })
        .await;
        assert!(
            listener_respawned,
            "the supervisor's handshake respawned the GET listener"
        );
        assert!(adapter.handshake_completed.load(Ordering::SeqCst));

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Same stale signal, other cancellable phase: delivered while the
    /// attempt is sleeping out its backoff. PR #163 review (round 5): the
    /// supervisor must re-check health and, finding the hint stale, resume
    /// the SAME wait — the original deadline is kept and no further attempt
    /// is recorded. Restarting the attempt would add a whole new backoff
    /// (60 s at the cap) on top of the one already mostly slept, against the
    /// "at most one capped backoff per attempt" requirement. With a 600 ms
    /// base the retained deadline lands at ~600 ms; a restarted attempt
    /// would land no earlier than 300 + 1200 ms.
    #[tokio::test]
    async fn stale_recovery_signal_during_backoff_keeps_the_deadline() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        *adapter.crash_tracker.lock().await = CrashTracker::new_test(
            Duration::from_millis(600),
            usize::MAX,
            Duration::from_secs(60),
        );
        let mut rx = adapter.subscribe_tools_changed().unwrap();

        let demoted_at = std::time::Instant::now();
        demote_via_transport_failures(&adapter).await;
        // Counter == 1: the attempt is recorded and its `recovered` future
        // (registered before the record) is live, so the signal reaches it.
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter
                    .crash_tracker
                    .try_lock()
                    .map(|t| t.consecutive_failures == 1)
                    .unwrap_or(false)
            })
            .await,
            "supervisor should have entered its backoff"
        );
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));

        // Halfway through the sleep: a stale hint.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);
        adapter.recovered_notify.notify_waiters();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            adapter.crash_tracker.lock().await.consecutive_failures,
            1,
            "a stale hint mid-sleep must not record another attempt"
        );
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));

        let recovered = wait_until(Duration::from_secs(5), || {
            adapter.health() == HealthStatus::Healthy
        })
        .await;
        let recovered_after = demoted_at.elapsed();
        assert!(recovered, "the pending attempt must recover the adapter");
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "one handshake, from the attempt that was already pending"
        );
        assert!(
            recovered_after < Duration::from_millis(1200),
            "a stale hint must not push the attempt past its original deadline \
             (recovered after {recovered_after:?}; a restarted backoff would need >= 1500 ms)"
        );
        assert_eq!(
            adapter.crash_tracker.lock().await.consecutive_failures,
            0,
            "the successful attempt resets the counter"
        );
        assert_eq!(drain_ticks(&mut rx), 1, "exactly one tick per recovery");

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 5): a stale hint is handled by re-reading health
    /// and re-arming the `Notified`. When the re-arm came AFTER the re-read, a
    /// genuine `Unhealthy → Healthy` flip landing between the two notified no
    /// registered waiter, so the supervisor slept its full backoff (5 s here)
    /// before noticing. The flip is injected deterministically from a tracing
    /// layer on the "stale recovery notification" event, i.e. exactly in that
    /// gap; the supervisor must stand down promptly, with no second handshake.
    #[test]
    fn genuine_recovery_in_stale_signal_gap_short_circuits_backoff() {
        use futures_util::FutureExt;
        use tracing::instrument::WithSubscriber;
        use tracing_subscriber::prelude::*;

        struct RecoverOnStale {
            adapter: HttpAdapter,
            fired: Arc<AtomicBool>,
        }
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecoverOnStale {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut msg = Message(String::new());
                event.record(&mut msg);
                if msg
                    .0
                    .contains("stale recovery notification while still Unhealthy")
                {
                    // The `health` read guard of the stale check is already
                    // released (the `if` temporary), so the write lock is
                    // uncontended here.
                    self.adapter
                        .note_request_success()
                        .now_or_never()
                        .expect("recovery locks are uncontended");
                    self.fired.store(true, Ordering::SeqCst);
                }
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (url, fx, server) = start_supervisor_fixture().await;
            let mut adapter = HttpAdapter::new(HttpConfig::new(url));
            adapter.initialize().await.expect("initialize succeeds");
            // Replace the supervisor with one running under the injecting
            // subscriber.
            if let Some(handle) = lock_slot(&adapter.reconnect_handle).take() {
                handle.abort();
            }
            *adapter.crash_tracker.lock().await =
                CrashTracker::new_test(Duration::from_secs(5), usize::MAX, Duration::from_secs(60));
            let fired = Arc::new(AtomicBool::new(false));
            let subscriber = tracing_subscriber::registry().with(RecoverOnStale {
                adapter: adapter.task_clone(),
                fired: fired.clone(),
            });
            let supervisor = adapter.task_clone();
            let supervisor_task = tokio::spawn(
                async move { supervisor.run_supervisor().await }.with_subscriber(subscriber),
            );
            let abort_handle = supervisor_task.abort_handle();
            *lock_slot(&adapter.reconnect_handle) = Some(supervisor_task);

            demote_via_transport_failures(&adapter).await;
            assert!(
                wait_until(Duration::from_secs(2), || {
                    adapter
                        .crash_tracker
                        .try_lock()
                        .map(|t| t.consecutive_failures == 1)
                        .unwrap_or(false)
                })
                .await,
                "supervisor should have entered its backoff"
            );

            // Stale hint (adapter still Unhealthy) → the layer recovers the
            // adapter for real inside the stale-check gap.
            adapter.recovered_notify.notify_waiters();
            assert!(
                wait_until(Duration::from_secs(2), || fired.load(Ordering::SeqCst)).await,
                "the stale hint must be observed"
            );
            assert_eq!(adapter.health(), HealthStatus::Healthy);

            let stood_down = wait_until(Duration::from_millis(300), || {
                adapter
                    .crash_tracker
                    .try_lock()
                    .map(|t| t.consecutive_failures == 0)
                    .unwrap_or(false)
            })
            .await;
            abort_handle.abort();
            assert_eq!(
                fx.init_count.load(Ordering::SeqCst),
                1,
                "a recovered adapter needs no supervisor handshake"
            );
            adapter.shutdown().await.unwrap();
            server.abort();
            assert!(
                stood_down,
                "genuine recovery in stale-check/rearm gap must short-circuit the pending sleep"
            );
        });
    }

    /// PR #163 review (round 4, Copilot): the dialect is subject to the same
    /// rule as the session — publish only what this attempt successfully
    /// negotiated. A supervisor attempt whose `server/discover` answered 2026
    /// but without a valid `serverInfo.name` fails at identity validation;
    /// if a caller's request recovered the legacy session just before, the
    /// conditional failure write leaves `Healthy`, and a dialect already
    /// switched to 2026 would format every later request as sessionless 2026
    /// traffic against the legacy session (400 from the upstream). Drives
    /// `complete_handshake` directly with the adapter already recovered —
    /// the deterministic equivalent of the recovery landing between the
    /// supervisor's post-probe health check and the identity failure.
    #[tokio::test]
    async fn obsolete_invalid_2026_discovery_keeps_legacy_dialect() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        assert!(!adapter.upstream_dialect().await.is_2026());
        assert_eq!(adapter.session_id.read().await.as_deref(), Some("sess-1"));

        // The adapter is Healthy on its legacy session when the obsolete
        // supervisor attempt processes an invalid 2026 discovery result.
        let invalid_2026 = json!({
            "protocolVersion": "2026-07-28",
            "capabilities": {"tools": {"listChanged": true}},
        });
        let err = adapter
            .complete_handshake(Some(invalid_2026), true)
            .await
            .expect_err("a 2026 discovery result without serverInfo.name is rejected");
        assert!(matches!(err, AdapterError::ProtocolError(_)), "{err:?}");

        assert_eq!(
            adapter.health(),
            HealthStatus::Healthy,
            "an obsolete attempt's failure never overwrites recovered health"
        );
        assert!(
            !adapter.upstream_dialect().await.is_2026(),
            "a rejected 2026 discovery must not publish the 2026 dialect"
        );
        assert_eq!(
            adapter.session_id.read().await.as_deref(),
            Some("sess-1"),
            "the working legacy session is untouched"
        );
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("later requests still run as legacy traffic on the working session");
        assert_eq!(fx.init_count.load(Ordering::SeqCst), 1);

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// PR #163 review (round 4, Copilot): `shutdown()` latches `shutting_down`
    /// and drains the supervisor slot, but a transport-failure demotion can
    /// enter `ensure_supervisor_running` after that drain. It must not
    /// install a new supervisor: that task would have missed the non-latched
    /// `shutdown_notify` and stayed parked after shutdown returned. Replays
    /// the interleaving: shutdown has set the flag and taken the handle, then
    /// the demotion lands.
    #[tokio::test]
    async fn transport_failure_racing_shutdown_installs_no_supervisor() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        assert!(lock_slot(&adapter.reconnect_handle).is_some());

        // shutdown() so far: flag latched, supervisor handle drained.
        adapter.shutting_down.store(true, Ordering::SeqCst);
        adapter.shutdown_notify.notify_waiters();
        let drained = lock_slot(&adapter.reconnect_handle).take();
        let drained = drained.expect("initialize armed a supervisor");
        drained.abort();
        let _ = drained.await;

        // The racing demotion (health is still Healthy at this point).
        demote_via_transport_failures(&adapter).await;
        assert!(
            lock_slot(&adapter.reconnect_handle).is_none(),
            "no supervisor may be installed once shutdown has begun"
        );

        // shutdown() completes normally and nothing is left behind.
        adapter.shutdown().await.expect("shutdown succeeds");
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert!(lock_slot(&adapter.reconnect_handle).is_none());
        assert!(lock_slot(&adapter.listener_handle).is_none());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            1,
            "no parked supervisor reconnects after shutdown"
        );

        server.abort();
    }

    /// PR #163 review (round 4, Copilot): the management enable path calls
    /// `initialize()` directly after `disable()`/`shutdown()` and propagates
    /// its error with `?`. Enabling a plain HTTP endpoint while its upstream
    /// is down must still leave a retrying adapter — `initialize()` arms the
    /// background retry on failure, the same contract as the startup path —
    /// so the endpoint recovers on its own when the upstream returns.
    #[tokio::test]
    async fn reinitialize_failure_after_shutdown_arms_recovery() {
        let (url, fx, server) = start_supervisor_fixture().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter.initialize().await.expect("initialize succeeds");
        adapter.shutdown().await.expect("shutdown succeeds");
        assert!(lock_slot(&adapter.reconnect_handle).is_none());

        // Enable while the upstream is down.
        fx.accepting.store(false, Ordering::SeqCst);
        adapter
            .initialize()
            .await
            .expect_err("re-initialize against a down upstream fails");
        assert!(matches!(adapter.health(), HealthStatus::Unhealthy(_)));
        assert!(
            lock_slot(&adapter.reconnect_handle)
                .as_ref()
                .is_some_and(|h| !h.is_finished()),
            "a failed re-initialize must arm the reconnect supervisor"
        );

        use_fast_backoff(&adapter).await;
        let mut rx = adapter.subscribe_tools_changed().expect("Some receiver");
        fx.accepting.store(true, Ordering::SeqCst);
        assert!(
            wait_until(Duration::from_secs(5), || {
                adapter.health() == HealthStatus::Healthy
            })
            .await,
            "the endpoint enabled while down must recover once the upstream returns"
        );
        assert_eq!(
            fx.init_count.load(Ordering::SeqCst),
            2,
            "recovery comes from the supervisor's handshake"
        );
        assert!(rx.try_recv().is_ok(), "recovery ticks tools_changed");
        adapter
            .call_tool("x", json!({}))
            .await
            .expect("the recovered endpoint is usable");

        adapter.shutdown().await.unwrap();
        server.abort();
    }

    /// Race guard (PR #123 review SHOULD-FIX): the failure-counter update and
    /// the `health` write are performed under a single `health` write lock, so
    /// concurrently firing a threshold-crossing transport failure and a
    /// success can never leave the adapter stuck `Unhealthy` with the counter
    /// below the threshold. Every settled state keeps counter and health in
    /// agreement: `Unhealthy("upstream unreachable")` implies the counter is at
    /// or above the threshold.
    #[tokio::test]
    async fn concurrent_success_and_failure_never_stick_unhealthy() {
        for _ in 0..1000 {
            let adapter = Arc::new(HttpAdapter::new(HttpConfig::new("http://127.0.0.1:1/mcp")));
            adapter.handshake_completed.store(true, Ordering::SeqCst);
            *adapter.health.write().await = HealthStatus::Healthy;
            // Prime the counter one below the threshold so the racing failure
            // would cross it and demote health.
            adapter
                .transport_failures
                .store(TRANSPORT_FAILURE_THRESHOLD - 1, Ordering::SeqCst);

            let fail = {
                let a = adapter.clone();
                tokio::spawn(async move { a.note_transport_failure().await })
            };
            let ok = {
                let a = adapter.clone();
                tokio::spawn(async move { a.note_request_success().await })
            };
            fail.await.unwrap();
            ok.await.unwrap();

            // Observe the settled state under the lock: counter and health must
            // agree. The buggy non-atomic version could leave counter == 0 while
            // health == Unhealthy.
            let health = adapter.health.read().await;
            let count = adapter.transport_failures.load(Ordering::SeqCst);
            if matches!(*health, HealthStatus::Unhealthy(_)) {
                assert!(
                    count >= TRANSPORT_FAILURE_THRESHOLD,
                    "stuck Unhealthy with counter {} below threshold {}",
                    count,
                    TRANSPORT_FAILURE_THRESHOLD
                );
            }
        }
    }

    /// A successful `initialize()` resets the transport-failure counter to 0,
    /// giving reactive health a clean slate after a (re)connect even if a prior
    /// run had accumulated transport-dead failures.
    #[tokio::test]
    async fn initialize_resets_transport_failure_counter() {
        // GET 405 so the test doesn't depend on the SSE channel; POST returns ok.
        let (url, server) = start_fake_http_server(true).await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        // Simulate stale state left over from a prior connection.
        adapter.transport_failures.store(5, Ordering::SeqCst);

        adapter.initialize().await.expect("initialize succeeds");

        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert_eq!(
            adapter.transport_failures.load(Ordering::SeqCst),
            0,
            "a successful initialize must reset the transport-failure counter"
        );

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

        // A pre-existing OBJECT `_meta` with sibling keys (e.g. W3C Trace
        // Context) is preserved; clientInfo is added alongside the siblings.
        let injected = HttpAdapter::inject_client_info(Some(json!({
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
        let injected = HttpAdapter::inject_client_info(Some(
            json!({"name": "echo", "_meta": "not-an-object"}),
        ))
        .unwrap();
        assert!(injected["_meta"].is_object());
        assert_eq!(
            injected["_meta"][protocol::META_CLIENT_INFO_KEY]["name"],
            "endara-relay"
        );
    }

    /// The 2026-07-28 spec ties `Mcp-Name` to `tools/call`: it must be emitted
    /// for that method (mirroring `params.name`) and never for another method
    /// that happens to carry a string `params.name`.
    #[test]
    fn test_2026_mcp_name_header_scoped_to_tools_call() {
        let client = reqwest::Client::new();
        let url = "http://localhost/mcp";

        // (a) tools/call with a string `name` still emits `Mcp-Name`.
        let req = HttpAdapter::apply_2026_headers(
            client.post(url),
            "tools/call",
            Some(&json!({"name": "echo", "arguments": {}})),
        )
        .build()
        .unwrap();
        assert_eq!(
            req.headers()
                .get(protocol::MCP_NAME_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("echo"),
            "tools/call emits Mcp-Name from params.name"
        );

        // (b) a non-tools/call method carrying a string `name` param does NOT
        // emit `Mcp-Name`.
        let req = HttpAdapter::apply_2026_headers(
            client.post(url),
            "tools/list",
            Some(&json!({"name": "echo"})),
        )
        .build()
        .unwrap();
        assert!(
            req.headers().get(protocol::MCP_NAME_HEADER).is_none(),
            "non-tools/call method does not emit Mcp-Name even with a string name param"
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

    // --- Multi round-trip (MRT) passthrough (T10) ---

    #[derive(Clone)]
    struct MrtState {
        seen: Arc<Mutex<Vec<Value>>>,
    }

    /// Fixture for the MRT round-trip: the first `tools/call` (no `requestState`)
    /// returns an `InputRequiredResult`; a follow-up carrying `requestState`
    /// returns the terminal `CallToolResult`. Every inbound `params` object is
    /// recorded so the test can assert what the relay forwarded.
    async fn dispatch_mrt(
        State(app): State<MrtState>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let params = body.get("params").cloned().unwrap_or(json!({}));
        app.seen.lock().await.push(params.clone());
        let result = if params.get("requestState").is_some() {
            json!({"content": [{"type": "text", "text": "done"}]})
        } else {
            json!({
                "inputRequests": [{"name": "city", "schema": {"type": "string"}}],
                "requestState": "state-xyz"
            })
        };
        Json(json!({"jsonrpc": "2.0", "result": result, "id": id})).into_response()
    }

    async fn start_mrt_server() -> (String, Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = MrtState { seen: seen.clone() };
        let app = Router::new()
            .route("/mcp", any(dispatch_mrt))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/mcp", addr);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, seen, handle)
    }

    /// A full multi round-trip: the relay forwards a terminal-shaped first call
    /// untouched, returns the upstream `InputRequiredResult` verbatim, and on the
    /// follow-up forwards `inputResponses`/`requestState` alongside
    /// `name`/`arguments` so the upstream can complete the call.
    #[tokio::test]
    async fn call_tool_round_trips_input_required_and_request_state() {
        let (url, seen, server) = start_mrt_server().await;
        let adapter = HttpAdapter::new(HttpConfig::new(url));

        // First call: no MRT siblings. Upstream returns an InputRequiredResult,
        // which must pass through unchanged (not coerced into a CallToolResult).
        let first = adapter
            .call_tool_with_request_params("ask", json!({"q": "?"}), serde_json::Map::new())
            .await
            .expect("first call_tool");
        assert_eq!(first["requestState"], "state-xyz");
        assert!(
            first.get("inputRequests").is_some(),
            "InputRequiredResult passes through unchanged"
        );
        assert!(
            first.get("content").is_none(),
            "InputRequiredResult must not be mangled into a content result"
        );

        // Follow-up: client supplies inputResponses + the echoed requestState.
        let mut request_params = serde_json::Map::new();
        request_params.insert(
            "inputResponses".to_string(),
            json!([{"name": "city", "value": "NYC"}]),
        );
        request_params.insert("requestState".to_string(), json!("state-xyz"));
        let second = adapter
            .call_tool_with_request_params("ask", json!({"q": "?"}), request_params)
            .await
            .expect("follow-up call_tool");
        assert_eq!(second["content"][0]["text"], "done");

        let seen = seen.lock().await;
        assert_eq!(seen.len(), 2, "two upstream tools/call requests");

        // First request carries only name/arguments — byte-for-byte legacy shape.
        let first_params = &seen[0];
        assert_eq!(first_params["name"], "ask");
        assert_eq!(first_params["arguments"], json!({"q": "?"}));
        assert!(first_params.get("requestState").is_none());
        assert!(first_params.get("inputResponses").is_none());

        // Follow-up request forwards the MRT siblings verbatim.
        let followup_params = &seen[1];
        assert_eq!(followup_params["name"], "ask");
        assert_eq!(followup_params["arguments"], json!({"q": "?"}));
        assert_eq!(followup_params["requestState"], "state-xyz");
        assert_eq!(followup_params["inputResponses"][0]["value"], "NYC");

        drop(seen);
        server.abort();
    }

    // --- D13: W3C Trace Context propagation in `_meta` ---

    /// Fixture that records the full inbound `params._meta` of every POST and
    /// answers a 2026 `server/discover` probe so the adapter takes the stateless
    /// 2026 path. The `tools/call` result carries its own `_meta` Trace Context
    /// so the test can assert the response (upstream→client) direction too.
    #[derive(Clone)]
    struct TraceServerState {
        seen_meta: Arc<Mutex<Vec<Value>>>,
    }

    async fn dispatch_trace_2026(
        State(app): State<TraceServerState>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        let method = body["method"].as_str().unwrap_or("").to_string();
        let id = body["id"].as_u64();
        let meta = body
            .get("params")
            .and_then(|p| p.get("_meta"))
            .cloned()
            .unwrap_or(Value::Null);
        app.seen_meta.lock().await.push(meta);

        let result = match method.as_str() {
            "server/discover" => json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "trace-2026", "version": "1.0.0"},
                "tools": []
            }),
            "tools/list" => json!({
                "tools": [{
                    "name": "echo",
                    "description": "Echoes",
                    "inputSchema": {"type": "object"}
                }]
            }),
            // Terminal result carries its own W3C Trace Context under `_meta`
            // so the test can assert the relay surfaces upstream→client trace
            // fields back to the caller unmodified.
            "tools/call" => json!({
                "content": [{"type": "text", "text": "ok"}],
                "_meta": {
                    "traceparent": "00-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cccccccccccccccc-01",
                    "tracestate": "vendor=resp"
                }
            }),
            _ => {
                return Json(json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32601, "message": "Method not found"},
                    "id": id,
                }))
                .into_response();
            }
        };
        if id.is_some() {
            Json(json!({"jsonrpc": "2.0", "result": result, "id": id})).into_response()
        } else {
            (StatusCode::ACCEPTED, "").into_response()
        }
    }

    async fn start_trace_2026_server() -> (String, Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        let seen_meta = Arc::new(Mutex::new(Vec::new()));
        let state = TraceServerState {
            seen_meta: seen_meta.clone(),
        };
        let app = Router::new()
            .route("/mcp", any(dispatch_trace_2026))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/mcp", addr);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (url, seen_meta, handle)
    }

    /// D13 — W3C Trace Context propagation through the relay's `_meta`, BOTH
    /// directions, on the 2026 HTTP path:
    /// - forward (client→relay→upstream): inbound `_meta` trace keys
    ///   (`traceparent`/`tracestate`/`baggage`) are forwarded to the upstream
    ///   `tools/call` UNMODIFIED, as siblings of the relay's own `_meta`
    ///   clientInfo — the clientInfo injection must NOT clobber them.
    /// - response (upstream→relay→client): the upstream result's `_meta` trace
    ///   keys are surfaced back to the caller verbatim.
    #[tokio::test]
    async fn call_tool_propagates_w3c_trace_context_both_directions() {
        let (url, seen_meta, server) = start_trace_2026_server().await;
        let mut adapter = HttpAdapter::new(HttpConfig::new(url));
        adapter
            .initialize()
            .await
            .expect("2026 initialize succeeds");
        assert!(
            adapter.upstream_dialect().await.is_2026(),
            "upstream should be detected as 2026"
        );

        // Inbound client→relay `_meta` carrying W3C Trace Context.
        let mut request_params = serde_json::Map::new();
        request_params.insert(
            "_meta".to_string(),
            json!({
                "traceparent": "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dddddddddddddddd-01",
                "tracestate": "vendor=req",
                "baggage": "userId=42"
            }),
        );

        let result = adapter
            .call_tool_with_request_params("echo", json!({"message": "hi"}), request_params)
            .await
            .expect("call_tool");

        // Response direction: upstream `_meta` trace fields surface to caller.
        assert_eq!(
            result["_meta"]["traceparent"],
            "00-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-cccccccccccccccc-01"
        );
        assert_eq!(result["_meta"]["tracestate"], "vendor=resp");

        // Forward direction: the upstream tools/call request carried the inbound
        // trace keys UNMODIFIED alongside the relay clientInfo (no clobbering).
        let seen = seen_meta.lock().await;
        let call_meta = seen
            .iter()
            .rev()
            .find(|m| m.get("traceparent").is_some())
            .expect("a tools/call _meta carrying trace context was forwarded");
        assert_eq!(
            call_meta["traceparent"],
            "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dddddddddddddddd-01"
        );
        assert_eq!(call_meta["tracestate"], "vendor=req");
        assert_eq!(call_meta["baggage"], "userId=42");
        assert_eq!(
            call_meta[protocol::META_CLIENT_INFO_KEY]["name"],
            "endara-relay",
            "relay clientInfo must coexist with forwarded trace context"
        );

        drop(seen);
        server.abort();
    }
}
