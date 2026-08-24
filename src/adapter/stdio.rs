use super::server_name::{sanitize_server_name, ServerNameError};
use super::server_type_resolution::{effective_server_type, strip_mcp_server_suffix};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo, DISCOVER_PROBE_TIMEOUT};
use crate::container_stats::{self, ContainerStats, StatsSlot};
use crate::events::{
    annotations_from_value, current_request_context, ToolCallEvent, ToolCallEventBus,
};
use crate::jsonrpc::{self, JsonRpcResponse};
use crate::protocol::{self, detect_upstream_dialect, ProtocolVersion};
use crate::shell_env;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn, Instrument};

/// Default OCI image used for containerized stdio servers when the endpoint
/// does not specify a `container_image`.
pub const DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/endara-ai/mcp-runner:latest";

/// Short, user-facing message recorded on an [`AdapterError::ProcessCrashed`]
/// when a stdio child fails to start or exits unexpectedly. The child's actual
/// failure output is surfaced as individual `[stderr]` rows in the endpoint's
/// Logs tab (see the stderr reader and [`StdioAdapter::stderr_lines`]), so the
/// crash banner stays a single readable sentence instead of a stderr dump.
const CRASH_USER_MESSAGE: &str = "Server failed to start. See Logs tab for details.";

/// Reason recorded on [`HealthStatus::Unhealthy`] when the auto-respawn
/// supervisor detects a crash loop (3+ crashes within 60 seconds).
const UNHEALTHY_REASON: &str = "3+ crashes in 60 seconds";

/// Sentinel for [`StdioAdapter::pending_eof_gen`]: no stdout-reader EOF was
/// observed while a respawn supervisor was already running.
const NO_PENDING_EOF: u64 = u64::MAX;

/// Process isolation mode for a stdio endpoint.
///
/// `Container` spawns the server through a detected container runtime
/// (docker/podman); `None` spawns the command directly on the host. The
/// default is `None` (direct spawn), matching `EndpointConfig`'s
/// "omitted means none" default that the watcher resolves when it builds
/// the [`StdioConfig`] — pre-existing configs keep working unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationMode {
    Container,
    #[default]
    None,
}

impl IsolationMode {
    /// Resolve the raw `isolation` config value for a stdio endpoint.
    /// Only an explicit `"container"` containerizes; omitted (or anything
    /// other than `"container"`) means direct spawn, so pre-existing configs
    /// keep working unchanged on upgrade — invalid values are caught earlier
    /// as validation warnings.
    pub fn from_config_value(value: Option<&str>) -> Self {
        match value {
            Some("container") => IsolationMode::Container,
            _ => IsolationMode::None,
        }
    }
}

/// Configuration for spawning a STDIO MCP server.
#[derive(Debug, Clone, Default)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// Optional override for the advertised `server_type` name. See
    /// [`crate::adapter::server_type_resolution::effective_server_type`].
    pub server_type_override: Option<String>,
    /// Endpoint name (used as the `endpoint` field on the adapter's
    /// per-endpoint `tracing` span). Defaults to empty for direct test
    /// construction; production paths set this from `EndpointConfig::name`.
    pub endpoint_name: String,
    /// Isolation mode. Defaults to [`IsolationMode::None`] (direct spawn)
    /// for direct construction; the watcher sets this from the endpoint's
    /// `isolation` field (omitted = none).
    pub isolation: IsolationMode,
    /// OCI image used when `isolation` is `Container`. `None` means
    /// [`DEFAULT_CONTAINER_IMAGE`].
    pub container_image: Option<String>,
    /// Host bind mounts (`"/host/path:/container/path"`), container mode only.
    pub mounts: Vec<String>,
}

/// Container name for an endpoint: `endara-mcp-<sanitized-endpoint>`.
fn container_name_for(endpoint_name: &str) -> String {
    let sanitized =
        crate::prefix::sanitize_name(endpoint_name).unwrap_or_else(|| "endpoint".to_string());
    format!("endara-mcp-{}", sanitized)
}

/// Build the argument vector for `docker run` (or podman, same CLI shape):
/// `run -i --rm --name endara-mcp-<endpoint> [-v mount]* [-e K=V]* <image> <command> <args...>`.
/// Env vars are sorted by key for deterministic output.
fn build_container_run_args(config: &StdioConfig) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-i".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name_for(&config.endpoint_name),
    ];
    for mount in &config.mounts {
        args.push("-v".to_string());
        args.push(mount.clone());
    }
    let mut env_keys: Vec<&String> = config.env.keys().collect();
    env_keys.sort();
    for key in env_keys {
        args.push("-e".to_string());
        args.push(format!("{}={}", key, config.env[key]));
    }
    args.push(
        config
            .container_image
            .clone()
            .unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.to_string()),
    );
    args.push(config.command.clone());
    args.extend(config.args.iter().cloned());
    args
}

/// Decide what to actually spawn: the container runtime (when isolation is
/// `Container` and a runtime was detected) or the direct command (isolation
/// `None`, or fallback when no runtime is available).
fn resolve_spawn_plan(config: &StdioConfig, runtime: Option<&str>) -> (String, Vec<String>) {
    match runtime {
        Some(rt) if config.isolation == IsolationMode::Container => {
            (rt.to_string(), build_container_run_args(config))
        }
        _ => (config.command.clone(), config.args.clone()),
    }
}

/// Per-endpoint isolation outcome reported via the management API.
///
/// Records what the endpoint config asked for (`configured`) versus what the
/// last spawn actually did (`actual`), making the silent
/// configured-container → direct fallback visible. `runtime`,
/// `container_name` and `image` are present only when the spawn actually
/// went through a container runtime.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IsolationState {
    /// Configured isolation mode: `"container"` or `"none"`.
    pub configured: &'static str,
    /// Actual spawn outcome: `"container"` or `"direct"`.
    pub actual: &'static str,
    /// Detected runtime CLI name (`"docker"` / `"podman"`), container only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Container name (`endara-mcp-<endpoint>`), container only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    /// OCI image the container runs, container only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// Build the [`IsolationState`] matching what [`resolve_spawn_plan`] decided.
/// `runtime_kind` is the detected runtime's CLI name (`"docker"`/`"podman"`),
/// `None` when no runtime was found (or isolation was not requested).
fn resolve_isolation_state(config: &StdioConfig, runtime_kind: Option<&str>) -> IsolationState {
    let configured = match config.isolation {
        IsolationMode::Container => "container",
        IsolationMode::None => "none",
    };
    match runtime_kind {
        Some(kind) if config.isolation == IsolationMode::Container => IsolationState {
            configured,
            actual: "container",
            runtime: Some(kind.to_string()),
            container_name: Some(container_name_for(&config.endpoint_name)),
            image: Some(
                config
                    .container_image
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.to_string()),
            ),
        },
        _ => IsolationState {
            configured,
            actual: "direct",
            runtime: None,
            container_name: None,
            image: None,
        },
    }
}

/// Detect a usable container runtime CLI via the shared
/// [`crate::container_runtime`] detector (cached for the process lifetime).
/// Returns the absolute path to the docker/podman binary, or `None` when no
/// runtime — or a runtime without a usable CLI (socket-only) — is found.
pub(crate) fn detect_container_runtime() -> Option<String> {
    crate::container_runtime::detect_runtime()?
        .cli_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Ring buffer that stores the last N lines of stderr output.
#[derive(Debug)]
pub struct RingBuffer {
    lines: Vec<String>,
    capacity: usize,
    write_pos: usize,
    count: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: vec![String::new(); capacity],
            capacity,
            write_pos: 0,
            count: 0,
        }
    }

    pub fn push(&mut self, line: String) {
        self.lines[self.write_pos] = line;
        self.write_pos = (self.write_pos + 1) % self.capacity;
        if self.count < self.capacity {
            self.count += 1;
        }
    }

    pub fn lines(&self) -> Vec<&str> {
        if self.count < self.capacity {
            self.lines[..self.count]
                .iter()
                .map(|s| s.as_str())
                .collect()
        } else {
            let mut result = Vec::with_capacity(self.capacity);
            for i in 0..self.capacity {
                let idx = (self.write_pos + i) % self.capacity;
                result.push(self.lines[idx].as_str());
            }
            result
        }
    }

    #[allow(dead_code)] // Used in tests
    pub fn len(&self) -> usize {
        self.count
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Crash tracking for exponential backoff.
#[derive(Debug)]
struct CrashTracker {
    timestamps: Vec<Instant>,
    consecutive_crashes: u32,
    /// When the most recent crash was recorded. The consecutive-crash streak
    /// deliberately survives a successful post-respawn handshake (a server
    /// that handshakes and then crashes seconds later is still crash-looping)
    /// and only resets after the child stays up for a full stability
    /// interval — see [`Self::record_crash`].
    last_crash: Option<Instant>,
    /// Base unit of the backoff schedule (1 second in production). Tests
    /// shrink it (via `StdioAdapter::set_backoff_unit_for_test`) so the full
    /// 1-1-2-4-8-…-60 schedule and the stability interval — both expressed in
    /// units — can be walked in milliseconds instead of real minutes.
    backoff_unit: Duration,
}

impl CrashTracker {
    fn new() -> Self {
        Self {
            timestamps: Vec::new(),
            consecutive_crashes: 0,
            last_crash: None,
            backoff_unit: Duration::from_secs(1),
        }
    }

    /// Record a crash. Returns `(unhealthy, backoff)`: whether the adapter
    /// should be marked unhealthy (3+ crashes within 60 seconds), and how
    /// long the supervisor should wait before the next respawn attempt.
    ///
    /// The backoff is derived from the PRE-increment streak so the delays run
    /// 1s, 1s, 2s, 4s, 8s, then hold at the 60s cap — matching the documented
    /// schedule. The streak resets only when the previous crash is at least a
    /// stability interval old (2x the cap, so capped retries — which arrive
    /// ~60s apart — never self-reset), not on every successful handshake.
    fn record_crash(&mut self) -> (bool, Duration) {
        let now = Instant::now();
        if let Some(last) = self.last_crash {
            if now.duration_since(last) >= self.stability_interval() {
                self.consecutive_crashes = 0;
            }
        }
        let backoff = self.backoff_for(self.consecutive_crashes);
        self.consecutive_crashes += 1;
        self.last_crash = Some(now);
        self.timestamps.push(now);

        // Remove crashes older than 60 seconds
        let cutoff = now - Duration::from_secs(60);
        self.timestamps.retain(|t| *t >= cutoff);

        // If 3+ crashes in 60 seconds, mark unhealthy
        (self.timestamps.len() >= 3, backoff)
    }

    /// Backoff before the (streak+1)-th consecutive respawn attempt, in
    /// multiples of the backoff unit: 1, 1, 2, 4, 8, then capped at 60.
    fn backoff_for(&self, streak: u32) -> Duration {
        let units: u32 = match streak {
            0 | 1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 60,
        };
        self.backoff_unit * units
    }

    /// How long the child must stay up before the consecutive-crash streak
    /// resets: twice the backoff cap (120 units = 2 minutes in production).
    fn stability_interval(&self) -> Duration {
        self.backoff_unit * 120
    }

    fn reset(&mut self) {
        self.consecutive_crashes = 0;
        self.last_crash = None;
    }
}

/// Format the current UTC time as an RFC-3339 / ISO-8601 string with
/// millisecond precision (e.g. `2026-05-27T04:36:29.710Z`). Shared by all
/// three adapters' `call_tool` event timestamps so the overlay sees a
/// consistent format regardless of transport.
pub(crate) fn iso8601_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Calculate backoff duration from crash count (exposed for testing).
#[allow(dead_code)] // Used in tests
pub fn calculate_backoff(consecutive_crashes: u32) -> Duration {
    let secs = match consecutive_crashes {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        _ => 60,
    };
    Duration::from_secs(secs)
}

/// Map of pending JSON-RPC request IDs to their oneshot response senders.
type PendingRequests = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<String>>>>;

/// STDIO MCP adapter — spawns a child process and communicates via stdin/stdout.
///
/// All state is `Arc`-shared, so `Clone` produces a second handle onto the
/// SAME adapter (same child process, same health, same pending map). The
/// auto-respawn supervisor relies on this: the stdout reader task holds a
/// cloned handle and can drive a full respawn from `&self`.
#[derive(Clone)]
pub struct StdioAdapter {
    config: StdioConfig,
    child: Arc<Mutex<Option<Child>>>,
    stdin_writer: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    pending_requests: PendingRequests,
    stderr_buffer: Arc<RwLock<RingBuffer>>,
    health: Arc<RwLock<HealthStatus>>,
    request_id: Arc<AtomicU64>,
    crash_tracker: Arc<Mutex<CrashTracker>>,
    /// Sanitized server name from the MCP initialize response.
    server_type: Arc<RwLock<Option<String>>>,
    /// Upstream-derived server name (sanitized + suffix-stripped), captured
    /// before any `server_type_override` resolution. See
    /// [`McpAdapter::upstream_server_name`].
    upstream_server_name: Arc<RwLock<Option<String>>>,
    /// Broadcast sender used to fan out `notifications/tools/list_changed`
    /// observations to subscribers (the registry's listener loop). Capacity is
    /// 16; `SendError` (no subscribers) is intentionally ignored.
    tools_changed_tx: broadcast::Sender<()>,
    /// Per-endpoint tracing span. Every adapter method instruments its async
    /// body with this span so events carry `endpoint`/`transport` (and
    /// `server_type` once the MCP handshake completes).
    span: tracing::Span,
    /// Once-guard for the span's `server_type` field. `Span::record` appends
    /// each write to the span's field list, so recording on every
    /// [`Self::initialize`] call (e.g. across stdio respawns) grows the
    /// `endpoint{…}` header without bound. This flag is flipped the first
    /// time a non-empty `server_type` is written so subsequent handshakes
    /// skip the record call.
    server_type_recorded: Arc<AtomicBool>,
    /// Set at the top of [`Self::shutdown`] (before any teardown work) and
    /// cleared when [`Self::initialize`] is called again. The auto-respawn
    /// supervisor checks this flag at every decision point so an intentional
    /// shutdown — including the manual restart endpoint in `management.rs`,
    /// which shuts the old adapter down — always wins over an in-flight
    /// respawn, even when the flag flips mid-attempt.
    shutdown_requested: Arc<AtomicBool>,
    /// Lifecycle generation (epoch) counter. Bumped by [`Self::initialize`]
    /// and [`Self::shutdown`], and CAS-claimed by the respawn supervisor
    /// before each respawn attempt, so every spawned child — and its stdout
    /// reader — is tagged with the generation that created it. Stale actors
    /// (an EOF hook from a previous child, a supervisor parked in a backoff
    /// sleep across a shutdown→initialize restart, a raced health write)
    /// compare their captured generation against the current value and stand
    /// down on mismatch. This closes the restart race where a supervisor
    /// sleeping through the brief `shutdown_requested` window would wake and
    /// kill a freshly restarted healthy child, and the health-write races
    /// where a stale respawn could overwrite `Stopped`/`Healthy` state owned
    /// by a newer lifecycle.
    generation: Arc<AtomicU64>,
    /// Generation of a stdout-reader EOF that arrived while a supervisor was
    /// already running (see [`Self::maybe_start_respawn_supervisor`]).
    /// `NO_PENDING_EOF` means none. The finishing supervisor re-checks this
    /// after clearing `respawning` so an EOF landing in that window re-arms a
    /// fresh supervisor instead of being dropped.
    pending_eof_gen: Arc<AtomicU64>,
    /// True while an auto-respawn supervisor task is running for this
    /// adapter. The stdout reader's EOF hook uses `swap` on this flag so at
    /// most one supervisor exists at a time (each respawn attempt spawns a
    /// fresh reader whose own EOF must not start a second supervisor); an
    /// EOF observed while the flag is set is preserved via `pending_eof_gen`.
    /// Also read by [`Self::send_external_request`] to fail external calls
    /// fast while a respawn is in flight.
    respawning: Arc<AtomicBool>,
    /// Shared typed event bus for the desktop overlay's SSE stream. Set
    /// once by [`Self::set_event_bus`] from `main.rs`/`watcher.rs` after
    /// construction; `None` keeps `call_tool` silent (no events published)
    /// which is what the legacy unit tests and ad-hoc constructions want.
    /// Wrapped in `Arc<OnceLock<_>>` for symmetry with the other adapters
    /// (SSE / HTTP derive `Clone`) and to keep the setter `&self`.
    event_bus: Arc<OnceLock<ToolCallEventBus>>,
    /// Per-tool annotation cache populated from `list_tools()` responses so
    /// `call_tool` can attach hint metadata to the `started` event without
    /// re-querying the upstream server. Stored as the raw `annotations`
    /// JSON value so the [`annotations_from_value`] helper performs the
    /// MCP-spec key mapping at event-emission time.
    tool_annotations_cache: Arc<RwLock<HashMap<String, Option<Value>>>>,
    /// When the server was spawned through a container runtime, holds
    /// `(runtime_cli, container_name)` so shutdown can `rm -f` the container
    /// as a backstop (killing the CLI client may orphan the container).
    container: Arc<Mutex<Option<(String, String)>>>,
    /// Latest container stats sample written by the background stats poller.
    /// `None` for direct spawns, before the first sample, or when the last
    /// poll failed. Uses a `std::sync::RwLock` so the sync
    /// [`McpAdapter::container_stats`] accessor can read it without await.
    container_stats: StatsSlot,
    /// Background `stats --no-stream` poll loop for containerized spawns,
    /// aborted on shutdown / respawn.
    stats_poller_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Isolation outcome of the most recent spawn, reported via
    /// [`McpAdapter::isolation_state`]. `None` until the first spawn. Uses a
    /// `std::sync::RwLock` so the sync accessor can read it without await.
    isolation_state: Arc<std::sync::RwLock<Option<IsolationState>>>,
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
    // Background task handles
    _stderr_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    _stdout_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl StdioAdapter {
    /// Create a new StdioAdapter with the given configuration.
    pub fn new(config: StdioConfig) -> Self {
        let (tools_changed_tx, _) = broadcast::channel(16);
        let span = tracing::info_span!(
            "endpoint",
            endpoint = %config.endpoint_name,
            transport = "stdio",
            server_type = tracing::field::Empty,
        );
        Self {
            config,
            child: Arc::new(Mutex::new(None)),
            stdin_writer: Arc::new(Mutex::new(None)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            stderr_buffer: Arc::new(RwLock::new(RingBuffer::new(1000))),
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: Arc::new(AtomicU64::new(1)),
            crash_tracker: Arc::new(Mutex::new(CrashTracker::new())),
            server_type: Arc::new(RwLock::new(None)),
            upstream_server_name: Arc::new(RwLock::new(None)),
            tools_changed_tx,
            span,
            server_type_recorded: Arc::new(AtomicBool::new(false)),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            pending_eof_gen: Arc::new(AtomicU64::new(NO_PENDING_EOF)),
            respawning: Arc::new(AtomicBool::new(false)),
            event_bus: Arc::new(OnceLock::new()),
            tool_annotations_cache: Arc::new(RwLock::new(HashMap::new())),
            container: Arc::new(Mutex::new(None)),
            container_stats: Arc::new(std::sync::RwLock::new(None)),
            stats_poller_handle: Arc::new(Mutex::new(None)),
            isolation_state: Arc::new(std::sync::RwLock::new(None)),
            upstream_dialect: Arc::new(RwLock::new(ProtocolVersion::V2024_11_05)),
            list_ttl_ms: Arc::new(RwLock::new(None)),
            _stderr_handle: Arc::new(Mutex::new(None)),
            _stdout_handle: Arc::new(Mutex::new(None)),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Record `server_type` on the per-endpoint span at most once. `Span::record`
    /// appends each write to the span's field list, so recording on every
    /// [`Self::initialize`] call (e.g. across stdio respawns) grows the
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

    /// Record the upstream server's negotiated [`ProtocolVersion`]. Populated
    /// during the `initialize` handshake (T7); consumed by the 2026 outbound
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
    /// no `initialize` handshake — and stdio carries no HTTP headers, so
    /// identity travels per-request inside `_meta`.
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

    /// Spawn the child process and set up I/O pipes.
    ///
    /// The spawn is tagged with the lifecycle generation current at entry
    /// (callers — `initialize()` and the respawn supervisor — bump/claim the
    /// generation immediately before calling). If the generation changes
    /// mid-spawn (a concurrent `shutdown()` or `initialize()` took over the
    /// lifecycle), the freshly spawned child is torn back down instead of
    /// being installed where nothing would ever reap it.
    async fn spawn_process(&self) -> Result<(), AdapterError> {
        let spawn_gen = self.generation.load(Ordering::SeqCst);
        {
            // Checked under the write lock so a shutdown() that lands between
            // the check and the write can't have its `Stopped` clobbered by a
            // stale `Starting` (shutdown stores its flag before writing).
            // On the supervisor's respawn path an `Unhealthy(reason)` is kept
            // so a crash-looping endpoint reads steadily as unhealthy instead
            // of flickering Unhealthy → Starting → Unhealthy once per backoff
            // (the write still happens when the endpoint was Healthy — that
            // transition is what arms the external-call fail-fast gate).
            let mut health = self.health.write().await;
            let keep_unhealthy = self.respawning.load(Ordering::SeqCst)
                && matches!(*health, HealthStatus::Unhealthy(_));
            if !self.shutdown_requested.load(Ordering::SeqCst) && !keep_unhealthy {
                *health = HealthStatus::Starting;
            }
        }

        // Resolve the container runtime when isolation is requested. No
        // runtime found → fall back to direct spawn (never hard-fail an
        // endpoint because Docker/Podman is absent).
        let runtime = if self.config.isolation == IsolationMode::Container {
            let rt = detect_container_runtime();
            if rt.is_none() {
                warn!(
                    "no container runtime (docker/podman) detected — falling back to direct \
                     spawn; install Docker or Podman to run this server isolated"
                );
            }
            rt
        } else {
            None
        };

        if let Some(ref rt) = runtime {
            // Best-effort removal of a stale container left over from a
            // previous run (e.g. after a crash where `--rm` didn't fire).
            let name = container_name_for(&self.config.endpoint_name);
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                Command::new(rt)
                    .args(["rm", "-f", &name])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            )
            .await;
        }

        let (program, argv) = resolve_spawn_plan(&self.config, runtime.as_deref());

        let mut cmd = Command::new(&program);
        cmd.args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if runtime.is_none() {
            // Direct spawn: inject the user's login-shell PATH so that
            // commands installed via nvm, Homebrew, pyenv, etc. are
            // discoverable even when the relay runs as a Tauri sidecar with
            // a minimal inherited environment.
            if let Some(shell_path) = shell_env::resolve_shell_path() {
                if !self.config.env.contains_key("PATH") {
                    cmd.env("PATH", shell_path);
                }
            }

            // User-specified env vars always win (applied after shell PATH).
            cmd.envs(&self.config.env);
        }
        // Container spawn: env vars are passed into the container via `-e`
        // args (see build_container_run_args); the runtime CLI itself runs
        // with the relay's own environment.

        let mut child = cmd
            .spawn()
            .map_err(|e| AdapterError::ProcessSpawnFailed(format!("{}: {}", program, e)))?;

        *self.container.lock().await = runtime
            .as_ref()
            .map(|rt| (rt.clone(), container_name_for(&self.config.endpoint_name)));

        // Record what this spawn actually did (container vs direct, including
        // the configured-container → direct fallback) for the management API.
        let runtime_kind = runtime
            .as_ref()
            .and_then(|_| crate::container_runtime::detect_runtime())
            .map(|info| info.kind.cli_name());
        if let Ok(mut state) = self.isolation_state.write() {
            *state = Some(resolve_isolation_state(&self.config, runtime_kind));
        }

        // (Re)start the stats poller for containerized spawns. Clear any
        // previous sample first so a respawn never serves stale stats.
        if let Ok(mut stats) = self.container_stats.write() {
            *stats = None;
        }
        {
            let mut handle = self.stats_poller_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }
            if let Some(rt) = runtime.as_ref() {
                *handle = Some(container_stats::spawn_stats_poller(
                    rt.clone(),
                    container_name_for(&self.config.endpoint_name),
                    self.container_stats.clone(),
                ));
            }
        }

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AdapterError::ProcessSpawnFailed("failed to capture stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::ProcessSpawnFailed("failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AdapterError::ProcessSpawnFailed("failed to capture stderr".into()))?;

        // Set up stdout line reader that dispatches by JSON-RPC response ID
        let pending = self.pending_requests.clone();
        let tools_changed_tx = self.tools_changed_tx.clone();
        // Cloned handle onto the same adapter (all state is Arc-shared) so
        // the reader's EOF path can kick the auto-respawn supervisor.
        let respawn_adapter = self.clone();
        let stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Try to parse as JSON and extract the "id" field
                let parsed: Result<Value, _> = serde_json::from_str(&line);
                match parsed {
                    Ok(obj) => {
                        if let Some(id) = obj.get("id").and_then(|v| v.as_u64()) {
                            // Response with an id — dispatch to the waiting caller
                            let sender = pending.lock().await.remove(&id);
                            match sender {
                                Some(tx) => {
                                    if tx.send(line).is_err() {
                                        debug!(id = id, "pending request receiver dropped");
                                    }
                                }
                                None => {
                                    warn!(id = id, "received response for unknown request id");
                                }
                            }
                        } else {
                            // Server notification (no id) — surface tools-changed
                            // ticks; debug-log everything else and drop.
                            match obj.get("method").and_then(|v| v.as_str()) {
                                Some("notifications/tools/list_changed") => {
                                    let _ = tools_changed_tx.send(());
                                }
                                _ => {
                                    debug!(line = %line, "MCP server notification (no id), dropping");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, line = %line, "non-JSON line from MCP server stdout");
                    }
                }
            }
            // Stdout closed (process exited) — drop all pending senders so
            // waiters immediately get a RecvError instead of hanging until
            // timeout. Gated on the generation (checked under the map lock)
            // so a stale reader from a previous process generation can never
            // clear entries registered by its successor: any entry inserted
            // by a newer lifecycle is preceded by a generation bump, which
            // this load would observe.
            {
                let mut map = pending.lock().await;
                if respawn_adapter.generation.load(Ordering::SeqCst) == spawn_gen && !map.is_empty()
                {
                    debug!(
                        count = map.len(),
                        "stdout closed, dropping pending requests"
                    );
                    map.clear();
                }
            }
            // Unexpected exit (not an intentional shutdown) — kick the
            // auto-respawn supervisor. No-op when shutdown was requested,
            // the EOF is from a stale process generation, or the child never
            // became healthy; preserved for later re-arm when a supervisor
            // already runs.
            respawn_adapter.maybe_start_respawn_supervisor(spawn_gen);
        });

        // Set up the stderr reader. Each line is BOTH pushed into the ring
        // buffer (the Logs-tab historical seed served by `stderr_lines`) AND
        // emitted as a WARN tracing event inside the endpoint span, so it
        // streams live into the desktop Logs tab as its own `[stderr]` row,
        // interleaved with the relay's own tracing lines right where the user
        // looks for the failure reason. Instrumenting with the endpoint span is
        // what tags the line with `endpoint=NAME` so it survives the
        // StdioAdapter → FailedAdapter swap on an init crash.
        let stderr_buf = self.stderr_buffer.clone();
        let stderr_span = self.span.clone();
        let stderr_handle = tokio::spawn(
            async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!("[stderr] {}", line);
                    stderr_buf.write().await.push(line);
                }
            }
            .instrument(stderr_span),
        );

        {
            let mut child_slot = self.child.lock().await;
            if self.generation.load(Ordering::SeqCst) != spawn_gen {
                // shutdown()/initialize() claimed the lifecycle while we were
                // spawning: this child belongs to a dead generation. Tear it
                // (and the state created above) back down instead of
                // installing it where nothing would ever reap it.
                drop(child_slot);
                stdout_handle.abort();
                stderr_handle.abort();
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                if let Some(h) = self.stats_poller_handle.lock().await.take() {
                    h.abort();
                }
                if let Some((rt, name)) = self.container.lock().await.take() {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        Command::new(&rt)
                            .args(["rm", "-f", &name])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status(),
                    )
                    .await;
                }
                return Err(AdapterError::ProcessSpawnFailed(
                    "adapter lifecycle changed during spawn".into(),
                ));
            }
            *child_slot = Some(child);
            *self.stdin_writer.lock().await = Some(stdin);
            *self._stdout_handle.lock().await = Some(stdout_handle);
            *self._stderr_handle.lock().await = Some(stderr_handle);
        }

        info!(command = %self.config.command, "MCP server process spawned");
        Ok(())
    }

    /// Wait briefly for the stderr reader task to drain to EOF (the child has
    /// exited, so its stderr pipe is closed and the reader will finish).
    ///
    /// Awaiting the reader closes the race where the child exits and the stdout
    /// side reports the crash before the final stderr lines — which usually
    /// carry the real failure reason — have been emitted into the Logs tab and
    /// pushed into the ring buffer. The wait is bounded so a still-open stderr
    /// pipe can't stall the caller on a non-fatal write error.
    async fn await_stderr_flush(&self) {
        if let Some(handle) = self._stderr_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_millis(500), handle).await;
        }
    }

    /// Build an [`AdapterError::ProcessCrashed`] carrying the short, user-facing
    /// [`CRASH_USER_MESSAGE`]. The child's actual stderr is surfaced as
    /// individual `[stderr]` rows in the endpoint's Logs tab, so the crash
    /// banner stays one readable sentence instead of a multi-line stderr dump.
    fn crashed_error() -> AdapterError {
        AdapterError::ProcessCrashed(CRASH_USER_MESSAGE.to_string())
    }

    /// Send a JSON-RPC request on behalf of an EXTERNAL caller (the
    /// `McpAdapter` trait methods: tool calls, list/read requests).
    ///
    /// While the auto-respawn supervisor is mid-respawn the child is either
    /// dead, in backoff, or has not finished its MCP handshake — an external
    /// request written to its stdin would sit on the 30s request timeout (or
    /// hit a protocol error from an uninitialized server). Fail fast with the
    /// crash banner instead. The supervisor's own handshake traffic calls
    /// [`Self::send_request`] directly and is not gated; once the
    /// post-respawn handshake flips health back to `Healthy`, external
    /// traffic resumes even if the supervisor task is still finishing up.
    async fn send_external_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        if self.respawning.load(Ordering::SeqCst)
            && !matches!(*self.health.read().await, HealthStatus::Healthy)
        {
            return Err(Self::crashed_error());
        }
        self.send_request(method, params).await
    }

    /// Send a JSON-RPC request and wait for the response.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
        // 2026 upstreams: every request carries the relay's `clientInfo` under
        // `params._meta` (there is no handshake). stdio has no HTTP headers, so
        // version/identity travel entirely in `_meta`. Legacy: unchanged.
        let params = if self.upstream_dialect.read().await.is_2026() {
            Self::inject_client_info(params)
        } else {
            params
        };

        let id = self.next_id();
        let request = jsonrpc::new_request(method, params, id);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        // Create a oneshot channel for this request's response
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();

        // Register the pending request before writing to stdin
        self.pending_requests.lock().await.insert(id, tx);

        // Write to stdin
        {
            let mut writer_guard = self.stdin_writer.lock().await;
            let writer = writer_guard.as_mut().ok_or_else(|| {
                // Clean up pending entry on error
                let pending = self.pending_requests.clone();
                let req_id = id;
                tokio::spawn(async move {
                    pending.lock().await.remove(&req_id);
                });
                AdapterError::NotInitialized
            })?;
            if let Err(e) = writer.write_all(line.as_bytes()).await {
                self.pending_requests.lock().await.remove(&id);
                debug!(error = %e, "stdin write failed");
                self.await_stderr_flush().await;
                return Err(Self::crashed_error());
            }
            if let Err(e) = writer.flush().await {
                self.pending_requests.lock().await.remove(&id);
                debug!(error = %e, "stdin flush failed");
                self.await_stderr_flush().await;
                return Err(Self::crashed_error());
            }
        }

        // Await the response with timeout (lock is NOT held during await)
        let response_line = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(line)) => line,
            Ok(Err(_)) => {
                // Sender was dropped (stdout reader shut down) — the child has
                // exited. Its recent stderr (the likely cause) flows into the
                // endpoint's Logs tab; wait for that to drain, then surface the
                // short crash banner.
                self.pending_requests.lock().await.remove(&id);
                debug!("stdout channel closed");
                self.await_stderr_flush().await;
                return Err(Self::crashed_error());
            }
            Err(_) => {
                // Timeout — clean up the pending entry
                self.pending_requests.lock().await.remove(&id);
                return Err(AdapterError::Timeout(30));
            }
        };

        let response: JsonRpcResponse = serde_json::from_str(&response_line).map_err(|e| {
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

    /// Send a JSON-RPC notification (no id, no response expected).
    ///
    /// Writes a single newline-terminated frame to the child's stdin. Unlike
    /// [`Self::send_request`], this does not allocate a request id, does not
    /// register a pending entry, and does not wait for a response.
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

        let notification = jsonrpc::new_notification(method, params);
        let mut line = serde_json::to_string(&notification)?;
        line.push('\n');

        let mut writer_guard = self.stdin_writer.lock().await;
        let writer = writer_guard.as_mut().ok_or(AdapterError::NotInitialized)?;
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            debug!(error = %e, "stdin write failed");
            self.await_stderr_flush().await;
            return Err(Self::crashed_error());
        }
        if let Err(e) = writer.flush().await {
            debug!(error = %e, "stdin flush failed");
            self.await_stderr_flush().await;
            return Err(Self::crashed_error());
        }
        Ok(())
    }

    /// Stateless `server/discover` probe used to detect a 2026 upstream before
    /// the legacy `initialize` handshake. The request carries the relay's
    /// `_meta` clientInfo (stdio has no HTTP headers, so version/identity travel
    /// entirely in `params._meta`). Returns the JSON-RPC `result` object on
    /// success, or `None` on any failure (JSON-RPC error, transport failure,
    /// missing result) so the caller falls back to the legacy handshake. Legacy
    /// servers reject `server/discover` (e.g. method-not-found / request before
    /// initialize) and the relay falls back transparently.
    async fn try_discover_probe(&self) -> Option<Value> {
        // Build params with `_meta` clientInfo explicitly: the upstream dialect
        // is still the legacy default here, so `send_request` would not inject
        // it for us, and a 2026 server expects identity on every request.
        let params = Self::inject_client_info(None);
        // Bound the probe with a short dedicated timeout (instead of inheriting
        // `send_request`'s 30s default) so a legacy upstream that silently drops
        // the unknown request falls back to the legacy handshake fast. A timeout
        // maps to `None`, the same clean legacy fallback as any other failure.
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
    /// 2026 stateless path so both name the endpoint identically.
    async fn apply_server_identity(&self, result: &Value) -> Result<(), AdapterError> {
        // Extract serverInfo.name — REQUIRED per MCP spec enforcement
        let raw_name = result
            .get("serverInfo")
            .and_then(|si| si.get("name"))
            .and_then(|n| n.as_str())
            .ok_or_else(|| {
                let err = ServerNameError::Missing;
                error!(error = %err, "MCP server did not provide serverInfo.name");
                AdapterError::ProtocolError(err.to_string())
            })?;

        // Validate and sanitize the server name
        let sanitized = sanitize_server_name(raw_name).map_err(|e| {
            error!(raw_name = %raw_name, error = %e, "serverInfo.name validation failed");
            AdapterError::ProtocolError(e.to_string())
        })?;

        // Resolve the effective `server_type` from the optional per-endpoint
        // override and the upstream-stripped sanitized name. Log a warning if
        // the override was supplied but failed sanitization (the resolver
        // falls back to the upstream-stripped name in that case).
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

        info!(raw_name = %raw_name, sanitized = %sanitized, effective = ?effective, "MCP server reported serverInfo.name");
        if let Some(ref name) = effective {
            self.record_server_type_once(name);
        }
        *self.server_type.write().await = effective;
        *self.upstream_server_name.write().await = Some(upstream_stripped);
        Ok(())
    }

    /// Perform the MCP initialize handshake.
    ///
    /// This method enforces that the server MUST provide a valid `serverInfo.name`
    /// in the initialize response. If the name is missing, empty, or reduces to
    /// empty after sanitization, the handshake fails with a ProtocolError.
    ///
    /// On success, a `notifications/initialized` notification is sent to the
    /// server (per the MCP spec) before returning. A failure to send that
    /// notification is logged at `warn!` level but does not fail the handshake.
    async fn mcp_initialize(&self) -> Result<(), AdapterError> {
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
            self.apply_server_identity(result).await?;
            // 2026 is stateless: no notifications/initialized handshake.
            info!("MCP initialize skipped (2026 stateless path)");
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

        let result = self.send_request("initialize", Some(params)).await?;

        // Validate + record the upstream serverInfo.name (REQUIRED per MCP spec
        // enforcement). Shared with the 2026 stateless path above.
        self.apply_server_identity(&result).await?;

        // Detect and record the upstream's negotiated protocol dialect. The
        // discover probe ran above (legacy result or none) and the initialize
        // result carries the negotiated legacy version; neither is 2026 here.
        self.set_upstream_dialect(detect_upstream_dialect(
            discover_result.as_ref(),
            Some(&result),
        ))
        .await;

        // Per the MCP spec the client MUST send a notifications/initialized
        // notification after a successful initialize exchange. Strict servers
        // refuse all subsequent requests until they observe it.
        if let Err(e) = self
            .send_notification("notifications/initialized", None)
            .await
        {
            warn!(error = %e, "failed to send notifications/initialized");
        }

        info!("MCP initialize handshake complete");
        Ok(())
    }

    /// Start the auto-respawn supervisor task for the process generation
    /// whose stdout reader observed EOF, unless shutdown was requested, the
    /// EOF is stale (a newer lifecycle already replaced that process), or a
    /// supervisor is already running — in which case the EOF is preserved in
    /// `pending_eof_gen` so the finishing supervisor re-arms for it instead
    /// of dropping it. Called from the stdout reader's EOF path, which fires
    /// exactly once per child process when its stdout pipe closes (i.e. the
    /// process exited).
    ///
    /// Deliberately NOT `async`: the stdout reader's future is part of
    /// `spawn_process`'s opaque return type, so awaiting a method here that
    /// (transitively) re-enters `spawn_process` would form an async
    /// opaque-type cycle the compiler rejects. Spawning from a sync method
    /// keeps the supervisor's future out of the reader's type.
    fn maybe_start_respawn_supervisor(&self, eof_gen: u64) {
        if self.shutdown_requested.load(Ordering::SeqCst) {
            debug!("process exited during shutdown, not respawning");
            return;
        }
        if self.generation.load(Ordering::SeqCst) != eof_gen {
            debug!("stale EOF from a previous process generation, ignoring");
            return;
        }
        // At most one supervisor per adapter: each respawn attempt spawns a
        // fresh stdout reader whose own EOF path lands here again while the
        // supervisor loop is still driving retries.
        if self.respawning.swap(true, Ordering::SeqCst) {
            // A supervisor is already running. Preserve this EOF: the
            // finishing supervisor re-checks `pending_eof_gen` after
            // clearing `respawning`, so a replacement child that handshakes
            // and then dies before the flag clears is re-supervised instead
            // of being silently dropped.
            self.pending_eof_gen.store(eof_gen, Ordering::SeqCst);
            // Close the race where the supervisor cleared the flag between
            // our first swap and the store above (it would have missed the
            // pending EOF): try to claim the slot again. If it is still (or
            // again) taken, whoever holds it is guaranteed to observe the
            // pending value on exit.
            if self.respawning.swap(true, Ordering::SeqCst) {
                return;
            }
            self.pending_eof_gen.store(NO_PENDING_EOF, Ordering::SeqCst);
        }
        let adapter = self.clone();
        let span = self.span.clone();
        tokio::spawn(
            async move {
                // Only supervise a child that completed its MCP handshake
                // (health Healthy). A crash BEFORE that point surfaces as an
                // `initialize` error to the caller (watcher/management),
                // which swaps in a FailedAdapter and drops this one — a
                // supervisor here would keep respawning a process nobody is
                // registered to use.
                //
                // INVARIANT this gate relies on: between the EOF and this
                // read, nothing else writes `health` — today only
                // `initialize`/`shutdown` (which bump the generation, making
                // this EOF stale) and the supervisor itself (at most one,
                // guaranteed by `respawning`) ever write it. If a future
                // change makes tool-call failures write health (as the HTTP
                // adapter does), this gate would silently stop respawning
                // after a crash observed first by a tool call.
                if matches!(*adapter.health.read().await, HealthStatus::Healthy) {
                    adapter.run_respawn_supervisor(eof_gen).await;
                } else {
                    debug!("process exited before becoming healthy, not respawning");
                }
                adapter.respawning.store(false, Ordering::SeqCst);
                // An EOF that arrived while this supervisor ran was preserved
                // above — re-arm for it now that the flag is clear (the gen
                // check in the recursive call discards it if stale).
                let pending = adapter
                    .pending_eof_gen
                    .swap(NO_PENDING_EOF, Ordering::SeqCst);
                if pending != NO_PENDING_EOF {
                    adapter.maybe_start_respawn_supervisor(pending);
                }
            }
            .instrument(span),
        );
    }

    /// Respawn loop: record the crash, back off exponentially (1s → 1s → 2s →
    /// 4s → 8s → 60s cap), then respawn and re-run the MCP handshake. 3+
    /// crashes within 60s mark the endpoint Unhealthy, but retries continue
    /// indefinitely at the capped interval until success or intentional
    /// shutdown — an Unhealthy endpoint recovers on its own when the
    /// underlying failure clears.
    ///
    /// `supervisor_gen` is the generation of the dead child this supervisor
    /// was armed for. Before each respawn attempt the loop CAS-claims the
    /// next generation; a failed CAS (or any observed generation change)
    /// means `shutdown()`/`initialize()` took over the lifecycle — e.g. the
    /// manual-restart path restarting the endpoint while this loop was
    /// parked in a backoff sleep — and the supervisor stands down instead of
    /// killing the newer lifecycle's healthy child.
    async fn run_respawn_supervisor(&self, mut supervisor_gen: u64) {
        loop {
            if self.shutdown_requested.load(Ordering::SeqCst) {
                debug!("respawn supervisor exiting: shutdown requested");
                return;
            }
            if self.generation.load(Ordering::SeqCst) != supervisor_gen {
                debug!("respawn supervisor exiting: adapter lifecycle changed");
                return;
            }

            let (unhealthy, backoff) = self.crash_tracker.lock().await.record_crash();
            if unhealthy {
                warn!(
                    backoff = ?backoff,
                    "server crash-looping, marking unhealthy (respawn attempts continue): {}",
                    UNHEALTHY_REASON
                );
                self.write_health_if_current(
                    supervisor_gen,
                    HealthStatus::Unhealthy(UNHEALTHY_REASON.to_string()),
                )
                .await;
            } else {
                warn!(
                    backoff = ?backoff,
                    "server process exited unexpectedly, respawning after backoff"
                );
            }

            tokio::time::sleep(backoff).await;

            if self.shutdown_requested.load(Ordering::SeqCst) {
                debug!("respawn supervisor exiting: shutdown requested during backoff");
                return;
            }
            // Claim the next lifecycle generation for this attempt. A failed
            // CAS means shutdown()/initialize() bumped it while we slept.
            let next_gen = supervisor_gen + 1;
            if self
                .generation
                .compare_exchange(supervisor_gen, next_gen, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                debug!("respawn supervisor exiting: lifecycle changed during backoff");
                return;
            }
            supervisor_gen = next_gen;

            match self.respawn_once(supervisor_gen).await {
                Ok(()) => {
                    if self.shutdown_requested.load(Ordering::SeqCst)
                        || self.generation.load(Ordering::SeqCst) != supervisor_gen
                    {
                        // Shutdown raced with the respawn. The fresh child
                        // was installed under our generation BEFORE the
                        // shutdown bump (spawn_process's generation-guarded
                        // store tears down any later spawn), so shutdown()'s
                        // own take-and-kill reaps it; likewise the guarded
                        // health write in respawn_once cannot clobber
                        // shutdown's `Stopped`. Just stand down quietly.
                        debug!("respawn supervisor exiting: shutdown raced with respawn");
                        return;
                    }
                    info!("server process respawned and re-initialized");
                    // Post-restart handshake succeeded — tick tools-changed so
                    // the registry refreshes its tool cache (the restarted
                    // server may advertise a different catalogue).
                    let _ = self.tools_changed_tx.send(());
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "respawn attempt failed, retrying");
                }
            }
        }
    }

    /// One respawn attempt: reap the dead child, join its stdout reader,
    /// respawn, and re-run the MCP handshake. Mirrors
    /// [`McpAdapter::initialize`] but works from `&self` (all adapter state
    /// is Arc-shared) so the supervisor task can drive it.
    ///
    /// The consecutive-crash streak is deliberately NOT reset on a successful
    /// handshake — a server that handshakes and then crashes seconds later is
    /// still crash-looping and must keep climbing toward the 60s backoff cap.
    /// The streak resets inside [`CrashTracker::record_crash`] once the child
    /// has stayed up for a full stability interval.
    async fn respawn_once(&self, attempt_gen: u64) -> Result<(), AdapterError> {
        self.kill_child_quiet().await;
        *self.stdin_writer.lock().await = None;
        // Join the previous generation's stdout reader before spawning the
        // replacement: its EOF cleanup clears `pending_requests`, so letting
        // it straggle past the new spawn could otherwise race requests
        // registered by the next generation (the clear is generation-gated
        // as a second line of defense). The child was killed above, so its
        // stdout pipe is closed and the reader finishes promptly.
        if let Some(handle) = self._stdout_handle.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        self.spawn_process().await?;
        self.mcp_initialize().await?;
        self.write_health_if_current(attempt_gen, HealthStatus::Healthy)
            .await;
        Ok(())
    }

    /// Write `status` into `health` only if the lifecycle generation still
    /// matches `expected_gen` (and no shutdown is in flight) at write time,
    /// checked under the write lock so a concurrent `shutdown()`/
    /// `initialize()` health write can never be clobbered by a stale respawn
    /// supervisor.
    async fn write_health_if_current(&self, expected_gen: u64, status: HealthStatus) {
        let mut health = self.health.write().await;
        if self.generation.load(Ordering::SeqCst) == expected_gen
            && !self.shutdown_requested.load(Ordering::SeqCst)
        {
            *health = status;
        }
    }

    /// Test-only: shrink the crash tracker's backoff unit so tests can walk
    /// the full backoff schedule (1-1-2-4-8-…-60 units) and the stability
    /// interval (120 units) in milliseconds instead of real minutes.
    #[doc(hidden)]
    #[allow(dead_code)] // Used in integration tests
    pub async fn set_backoff_unit_for_test(&self, unit: Duration) {
        self.crash_tracker.lock().await.backoff_unit = unit;
    }

    /// Test-only: kill the current child WITHOUT flagging shutdown,
    /// simulating an unexpected crash so tests can trigger the auto-respawn
    /// supervisor on demand.
    #[doc(hidden)]
    #[allow(dead_code)] // Used in integration tests
    pub async fn kill_child_for_test(&self) {
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = child.start_kill();
        }
    }

    /// Best-effort kill + reap of the current child, if any. Used by the
    /// respawn path to avoid leaking a zombie before spawning the replacement.
    async fn kill_child_quiet(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
    }
}

#[async_trait]
impl McpAdapter for StdioAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        async {
            // Claim a fresh lifecycle generation FIRST: any supervisor still
            // parked in a backoff sleep from the previous lifecycle (e.g.
            // across the manual restart endpoint's shutdown() → initialize()
            // sequence) fails its CAS on wake and stands down instead of
            // killing the child spawned below.
            let init_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            // Re-arm the auto-respawn supervisor: a previous shutdown() (e.g.
            // the manual restart endpoint re-initializing this adapter) set
            // the flag to suppress respawns of the old child.
            self.shutdown_requested.store(false, Ordering::SeqCst);
            self.spawn_process().await?;
            self.mcp_initialize().await?;
            self.write_health_if_current(init_gen, HealthStatus::Healthy)
                .await;
            self.crash_tracker.lock().await.reset();
            Ok(())
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        async {
            let result = self.send_external_request("tools/list", None).await?;
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
            // Refresh the per-tool annotations cache used by `call_tool` to
            // join hint metadata onto the overlay's `started` event. Mirrors
            // the registry's tool cache lifecycle: rewritten on every
            // successful list, never trimmed (the worst-case footprint is
            // one entry per advertised tool, which is bounded by the
            // upstream server's catalogue).
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
            let result = self.send_external_request("resources/list", None).await?;
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
            let result = self
                .send_external_request("resources/templates/list", None)
                .await?;
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
            self.send_external_request("resources/read", Some(params))
                .await
        }
        .instrument(self.span.clone())
        .await
    }

    async fn list_prompts(&self) -> Result<Vec<Value>, AdapterError> {
        async {
            let result = self.send_external_request("prompts/list", None).await?;
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
            self.send_external_request("prompts/get", Some(Value::Object(params)))
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
        // Pull JSON-RPC id and profile from the surrounding tracing spans
        // (`request{id=...}` / `mcp_request{profile=...}`) BEFORE re-entering
        // the adapter's own `endpoint` span — the endpoint span was created
        // at adapter construction time and is not parented to the per-request
        // span, so a `current_request_context()` call from inside the
        // `.instrument(self.span)` body would lose the caller's span scope.
        // See `events::SpanFieldCaptureLayer`.
        let span_ctx = current_request_context();
        async {
            let request_id = uuid::Uuid::new_v4().to_string();
            // Publish `started` before the network/process round-trip so the
            // overlay can spawn an in-flight card immediately.
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
                    transport: "stdio".into(),
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
            let result = self.send_external_request("tools/call", Some(params)).await;
            let duration_ms = start.elapsed().as_millis();
            // A transport-level `Ok` can still carry a tool-level error
            // envelope (`{ content: [...], isError: true }`). Surface it as a
            // failed call in the tracing line and overlay events — mirroring
            // the registry's durable capture — while forwarding the envelope
            // to the client unchanged.
            let tool_error = result
                .as_ref()
                .ok()
                .and_then(crate::adapter::tool_result_error_message);
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
        // Use try_read to avoid blocking; fall back to Starting
        match self.health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        }
    }

    fn container_stats(&self) -> Option<ContainerStats> {
        self.container_stats.read().ok().and_then(|g| (*g).clone())
    }

    fn isolation_state(&self) -> Option<IsolationState> {
        self.isolation_state.read().ok().and_then(|g| (*g).clone())
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

    async fn stderr_lines(&self) -> Vec<String> {
        // Tag each line with a WARN level and a `[stderr]` marker so the
        // desktop log parser renders captured child stderr as a distinct pill,
        // set apart from the relay's own tracing lines in the Logs tab.
        self.stderr_buffer
            .read()
            .await
            .lines()
            .iter()
            .map(|s| format!("WARN [stderr] {}", s))
            .collect()
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        async {
            // Flag intentional shutdown FIRST so the stdout reader's EOF hook
            // (fired when we kill the child below) and any in-flight respawn
            // supervisor stand down instead of respawning the process. Bump
            // the lifecycle generation too: a supervisor parked in a backoff
            // sleep across this whole shutdown (and a subsequent initialize)
            // fails its CAS on wake, a mid-spawn respawn attempt tears its
            // child back down instead of installing it, and any guarded
            // health write from the old lifecycle becomes a no-op.
            self.shutdown_requested.store(true, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
            *self.health.write().await = HealthStatus::Stopped;

            // Try graceful close via stdin
            if let Some(stdin) = self.stdin_writer.lock().await.take() {
                drop(stdin);
            }

            // Drop all pending request senders — waiting callers will get RecvError
            {
                let mut pending = self.pending_requests.lock().await;
                let count = pending.len();
                pending.clear();
                if count > 0 {
                    debug!(count = count, "dropped pending requests during shutdown");
                }
            }

            // Try to kill the child process
            if let Some(mut child) = self.child.lock().await.take() {
                // Send SIGTERM (kill on unix sends SIGKILL, so we use start_kill)
                let _ = child.start_kill();

                // Wait up to 5 seconds for graceful shutdown
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(status)) => {
                        info!(exit_code = ?status.code(), "MCP server exited");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "error waiting for MCP server exit");
                    }
                    Err(_) => {
                        warn!("MCP server did not exit within 5s, force killing");
                        let _ = child.kill().await;
                    }
                }
            }

            // Stop the stats poller and drop the last sample so a stopped
            // endpoint never reports stale container stats.
            if let Some(h) = self.stats_poller_handle.lock().await.take() {
                h.abort();
            }
            if let Ok(mut stats) = self.container_stats.write() {
                *stats = None;
            }

            // Containerized spawn: killing the runtime CLI client may orphan
            // the container, so remove it by name as a best-effort backstop.
            if let Some((runtime, name)) = self.container.lock().await.take() {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    Command::new(&runtime)
                        .args(["rm", "-f", &name])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status(),
                )
                .await;
            }

            // Abort background tasks
            if let Some(h) = self._stderr_handle.lock().await.take() {
                h.abort();
            }
            if let Some(h) = self._stdout_handle.lock().await.take() {
                h.abort();
            }

            info!("STDIO adapter shut down");
            Ok(())
        }
        .instrument(self.span.clone())
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic() {
        let mut buf = RingBuffer::new(3);
        buf.push("line1".into());
        buf.push("line2".into());
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.lines(), vec!["line1", "line2"]);
    }

    #[test]
    fn test_ring_buffer_overflow() {
        let mut buf = RingBuffer::new(3);
        buf.push("a".into());
        buf.push("b".into());
        buf.push("c".into());
        buf.push("d".into());
        assert_eq!(buf.len(), 3);
        // Oldest ("a") should be gone, order should be b, c, d
        assert_eq!(buf.lines(), vec!["b", "c", "d"]);
    }

    #[test]
    fn test_ring_buffer_capacity() {
        let mut buf = RingBuffer::new(1000);
        for i in 0..1500 {
            buf.push(format!("line {}", i));
        }
        assert_eq!(buf.len(), 1000);
        let lines = buf.lines();
        assert_eq!(lines[0], "line 500");
        assert_eq!(lines[999], "line 1499");
    }

    #[test]
    fn test_backoff_timing() {
        assert_eq!(calculate_backoff(0), Duration::from_secs(1));
        assert_eq!(calculate_backoff(1), Duration::from_secs(1));
        assert_eq!(calculate_backoff(2), Duration::from_secs(2));
        assert_eq!(calculate_backoff(3), Duration::from_secs(4));
        assert_eq!(calculate_backoff(4), Duration::from_secs(8));
        assert_eq!(calculate_backoff(5), Duration::from_secs(60));
        assert_eq!(calculate_backoff(100), Duration::from_secs(60));
    }

    #[test]
    fn test_health_status_transitions() {
        // Test display impl
        assert_eq!(HealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(HealthStatus::Starting.to_string(), "starting");
        assert_eq!(HealthStatus::Stopped.to_string(), "stopped");
        assert_eq!(
            HealthStatus::Unhealthy("test".into()).to_string(),
            "unhealthy: test"
        );
    }

    #[test]
    fn test_crash_tracker_marks_unhealthy_after_3_crashes() {
        let mut tracker = CrashTracker::new();
        assert!(!tracker.record_crash().0); // 1st crash
        assert!(!tracker.record_crash().0); // 2nd crash
        assert!(tracker.record_crash().0); // 3rd crash → unhealthy
    }

    #[test]
    fn test_crash_tracker_backoff_schedule() {
        // Delays are derived from the PRE-increment streak: 1, 1, 2, 4, 8,
        // then held at the 60s cap.
        let mut tracker = CrashTracker::new();
        let expected = [1u64, 1, 2, 4, 8, 60, 60, 60];
        for (i, secs) in expected.iter().enumerate() {
            let (_, backoff) = tracker.record_crash();
            assert_eq!(
                backoff,
                Duration::from_secs(*secs),
                "wrong backoff for crash #{}",
                i + 1
            );
        }
    }

    #[test]
    fn test_crash_tracker_reset() {
        let mut tracker = CrashTracker::new();
        tracker.record_crash();
        tracker.record_crash();
        tracker.reset();
        assert_eq!(tracker.record_crash().1, Duration::from_secs(1));
    }

    #[test]
    fn test_crash_tracker_streak_survives_quick_recovery() {
        // A crash arriving before the stability interval keeps climbing the
        // schedule even though a successful handshake happened in between
        // (the supervisor never calls reset() on respawn success).
        let mut tracker = CrashTracker::new();
        assert_eq!(tracker.record_crash().1, Duration::from_secs(1));
        assert_eq!(tracker.record_crash().1, Duration::from_secs(1));
        assert_eq!(tracker.record_crash().1, Duration::from_secs(2));
        assert_eq!(tracker.record_crash().1, Duration::from_secs(4));
    }

    #[test]
    fn test_crash_tracker_streak_resets_after_stability_interval() {
        // With a tiny backoff unit the stability interval (120 units) is
        // walkable in real time: after the child stays up that long, the
        // next crash restarts the schedule from the first step.
        let mut tracker = CrashTracker::new();
        tracker.backoff_unit = Duration::from_millis(1);
        assert_eq!(tracker.record_crash().1, Duration::from_millis(1));
        assert_eq!(tracker.record_crash().1, Duration::from_millis(1));
        assert_eq!(tracker.record_crash().1, Duration::from_millis(2));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(tracker.record_crash().1, Duration::from_millis(1));
    }

    /// Repeated `initialize` calls (stdio respawns re-run the handshake) must
    /// NOT re-append `server_type` to the per-endpoint span's field list. The
    /// once-guard flips on the first record and short-circuits every later
    /// call, preventing unbounded `endpoint{…}` log-header growth.
    #[test]
    fn record_server_type_once_guards_repeated_calls() {
        let adapter = StdioAdapter::new(StdioConfig::default());
        assert!(!adapter.server_type_recorded_flag());

        adapter.record_server_type_once("some-server");
        assert!(adapter.server_type_recorded_flag());

        // Second and subsequent calls (e.g. after a respawn re-runs
        // initialize) must NOT re-record — the guard has already flipped,
        // so this is a no-op.
        adapter.record_server_type_once("some-server");
        adapter.record_server_type_once("other-name");
        assert!(adapter.server_type_recorded_flag());
    }

    // -----------------------------------------------------------------------
    // Pending-requests dispatch tests
    // -----------------------------------------------------------------------

    /// Helper: create a pending-requests map and insert senders for the given IDs.
    /// Returns the map and the corresponding receivers.
    async fn make_pending(
        ids: &[u64],
    ) -> (
        PendingRequests,
        Vec<(u64, tokio::sync::oneshot::Receiver<String>)>,
    ) {
        let map: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let mut rxs = Vec::new();
        for &id in ids {
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            map.lock().await.insert(id, tx);
            rxs.push((id, rx));
        }
        (map, rxs)
    }

    // -----------------------------------------------------------------------
    // Containerized spawn plan (no docker needed — pure arg construction)
    // -----------------------------------------------------------------------

    #[test]
    fn test_isolation_mode_from_config_value() {
        // Omitted means direct spawn so pre-existing configs keep working
        // unchanged on upgrade; only an explicit "container" containerizes.
        assert_eq!(IsolationMode::from_config_value(None), IsolationMode::None);
        assert_eq!(
            IsolationMode::from_config_value(Some("container")),
            IsolationMode::Container
        );
        assert_eq!(
            IsolationMode::from_config_value(Some("none")),
            IsolationMode::None
        );
    }

    #[test]
    fn test_container_name_sanitizes_endpoint_name() {
        assert_eq!(container_name_for("my-server"), "endara-mcp-my-server");
        assert_eq!(container_name_for("My Server!"), "endara-mcp-my_server");
        assert_eq!(container_name_for("!!!"), "endara-mcp-endpoint");
    }

    #[test]
    fn test_build_container_run_args_full_shape() {
        let mut env = HashMap::new();
        env.insert("ZED".to_string(), "z".to_string());
        env.insert("API_KEY".to_string(), "secret".to_string());
        let config = StdioConfig {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "some-server".to_string()],
            env,
            endpoint_name: "github".to_string(),
            isolation: IsolationMode::Container,
            container_image: None,
            mounts: vec!["/host/a:/ctr/a".to_string(), "/host/b:/ctr/b".to_string()],
            ..Default::default()
        };
        let args = build_container_run_args(&config);
        assert_eq!(
            args,
            vec![
                "run",
                "-i",
                "--rm",
                "--name",
                "endara-mcp-github",
                "-v",
                "/host/a:/ctr/a",
                "-v",
                "/host/b:/ctr/b",
                "-e",
                "API_KEY=secret",
                "-e",
                "ZED=z",
                DEFAULT_CONTAINER_IMAGE,
                "npx",
                "-y",
                "some-server",
            ]
        );
    }

    #[test]
    fn test_build_container_run_args_custom_image_no_mounts_no_env() {
        let config = StdioConfig {
            command: "uvx".to_string(),
            args: vec!["mcp-fetch".to_string()],
            endpoint_name: "fetch".to_string(),
            isolation: IsolationMode::Container,
            container_image: Some("example.com/custom:1".to_string()),
            ..Default::default()
        };
        let args = build_container_run_args(&config);
        assert_eq!(
            args,
            vec![
                "run",
                "-i",
                "--rm",
                "--name",
                "endara-mcp-fetch",
                "example.com/custom:1",
                "uvx",
                "mcp-fetch",
            ]
        );
    }

    #[test]
    fn test_resolve_spawn_plan_container_with_runtime() {
        let config = StdioConfig {
            command: "npx".to_string(),
            args: vec!["server".to_string()],
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::Container,
            ..Default::default()
        };
        let (program, argv) = resolve_spawn_plan(&config, Some("/usr/local/bin/docker"));
        assert_eq!(program, "/usr/local/bin/docker");
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"npx".to_string()));
    }

    #[test]
    fn test_resolve_spawn_plan_falls_back_to_direct_without_runtime() {
        let config = StdioConfig {
            command: "npx".to_string(),
            args: vec!["server".to_string()],
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::Container,
            ..Default::default()
        };
        let (program, argv) = resolve_spawn_plan(&config, None);
        assert_eq!(program, "npx");
        assert_eq!(argv, vec!["server".to_string()]);
    }

    #[test]
    fn test_resolve_spawn_plan_omitted_isolation_spawns_directly() {
        // Omitted `isolation` resolves to direct spawn even when a container
        // runtime is available — legacy configs must not be containerized.
        let config = StdioConfig {
            command: "npx".to_string(),
            args: vec!["server".to_string()],
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::from_config_value(None),
            ..Default::default()
        };
        let (program, argv) = resolve_spawn_plan(&config, Some("/usr/local/bin/docker"));
        assert_eq!(program, "npx");
        assert_eq!(argv, vec!["server".to_string()]);
    }

    #[test]
    fn test_resolve_spawn_plan_isolation_none_ignores_runtime() {
        let config = StdioConfig {
            command: "npx".to_string(),
            args: vec!["server".to_string()],
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::None,
            ..Default::default()
        };
        let (program, argv) = resolve_spawn_plan(&config, Some("/usr/local/bin/docker"));
        assert_eq!(program, "npx");
        assert_eq!(argv, vec!["server".to_string()]);
    }

    // -----------------------------------------------------------------------
    // Isolation-state reporting (configured vs actual spawn outcome)
    // -----------------------------------------------------------------------

    #[test]
    fn test_isolation_state_container_with_runtime_reports_full_state() {
        let config = StdioConfig {
            command: "npx".to_string(),
            args: vec!["server".to_string()],
            endpoint_name: "github".to_string(),
            isolation: IsolationMode::Container,
            container_image: None,
            ..Default::default()
        };
        let state = resolve_isolation_state(&config, Some("docker"));
        assert_eq!(state.configured, "container");
        assert_eq!(state.actual, "container");
        assert_eq!(state.runtime, Some("docker".to_string()));
        assert_eq!(state.container_name, Some("endara-mcp-github".to_string()));
        assert_eq!(state.image, Some(DEFAULT_CONTAINER_IMAGE.to_string()));
    }

    #[test]
    fn test_isolation_state_container_reports_custom_image() {
        let config = StdioConfig {
            command: "uvx".to_string(),
            endpoint_name: "fetch".to_string(),
            isolation: IsolationMode::Container,
            container_image: Some("example.com/custom:1".to_string()),
            ..Default::default()
        };
        let state = resolve_isolation_state(&config, Some("podman"));
        assert_eq!(state.actual, "container");
        assert_eq!(state.runtime, Some("podman".to_string()));
        assert_eq!(state.image, Some("example.com/custom:1".to_string()));
    }

    #[test]
    fn test_isolation_state_direct_configured_reports_direct() {
        // isolation = none reports configured=none/actual=direct, with no
        // container details — even when a runtime happens to be available.
        let config = StdioConfig {
            command: "npx".to_string(),
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::None,
            ..Default::default()
        };
        let state = resolve_isolation_state(&config, None);
        assert_eq!(state.configured, "none");
        assert_eq!(state.actual, "direct");
        assert_eq!(state.runtime, None);
        assert_eq!(state.container_name, None);
        assert_eq!(state.image, None);
    }

    #[test]
    fn test_isolation_state_fallback_reports_configured_container_actual_direct() {
        // Configured container but no runtime detected: the silent fallback
        // to direct spawn must be visible as configured=container/actual=direct.
        let config = StdioConfig {
            command: "npx".to_string(),
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::Container,
            ..Default::default()
        };
        let state = resolve_isolation_state(&config, None);
        assert_eq!(state.configured, "container");
        assert_eq!(state.actual, "direct");
        assert_eq!(state.runtime, None);
        assert_eq!(state.container_name, None);
        assert_eq!(state.image, None);
    }

    #[test]
    fn test_isolation_state_serialization_omits_absent_container_fields() {
        let config = StdioConfig {
            command: "npx".to_string(),
            endpoint_name: "ep".to_string(),
            isolation: IsolationMode::Container,
            ..Default::default()
        };
        let fallback = serde_json::to_value(resolve_isolation_state(&config, None)).unwrap();
        assert_eq!(
            fallback,
            json!({ "configured": "container", "actual": "direct" })
        );
        let containerized =
            serde_json::to_value(resolve_isolation_state(&config, Some("docker"))).unwrap();
        assert_eq!(
            containerized,
            json!({
                "configured": "container",
                "actual": "container",
                "runtime": "docker",
                "container_name": "endara-mcp-ep",
                "image": DEFAULT_CONTAINER_IMAGE,
            })
        );
    }

    /// Simulate the stdout reader dispatch logic for a single line.
    async fn dispatch_line(pending: &PendingRequests, line: &str) -> DispatchResult {
        let parsed: Result<Value, _> = serde_json::from_str(line);
        match parsed {
            Ok(obj) => {
                if let Some(id) = obj.get("id").and_then(|v| v.as_u64()) {
                    let sender = pending.lock().await.remove(&id);
                    match sender {
                        Some(tx) => {
                            let _ = tx.send(line.to_string());
                            DispatchResult::Dispatched(id)
                        }
                        None => DispatchResult::UnknownId(id),
                    }
                } else {
                    DispatchResult::Notification
                }
            }
            Err(_) => DispatchResult::InvalidJson,
        }
    }

    #[derive(Debug, PartialEq)]
    enum DispatchResult {
        Dispatched(u64),
        UnknownId(u64),
        Notification,
        InvalidJson,
    }

    #[tokio::test]
    async fn test_dispatch_matching_id() {
        let (pending, mut rxs) = make_pending(&[1, 2]).await;
        let line = r#"{"jsonrpc":"2.0","result":{"ok":true},"id":1}"#;
        let result = dispatch_line(&pending, line).await;
        assert_eq!(result, DispatchResult::Dispatched(1));

        // Receiver for id=1 should have the line
        let (_, rx1) = rxs.remove(0);
        assert_eq!(rx1.await.unwrap(), line);

        // id=2 should still be pending
        assert!(pending.lock().await.contains_key(&2));
    }

    #[tokio::test]
    async fn test_dispatch_notification_no_id() {
        let (pending, _rxs) = make_pending(&[1]).await;
        let line = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        let result = dispatch_line(&pending, line).await;
        assert_eq!(result, DispatchResult::Notification);

        // Pending map should be unchanged
        assert!(pending.lock().await.contains_key(&1));
    }

    #[tokio::test]
    async fn test_dispatch_unknown_id() {
        let (pending, _rxs) = make_pending(&[1]).await;
        let line = r#"{"jsonrpc":"2.0","result":{},"id":999}"#;
        let result = dispatch_line(&pending, line).await;
        assert_eq!(result, DispatchResult::UnknownId(999));

        // id=1 still pending
        assert!(pending.lock().await.contains_key(&1));
    }

    #[tokio::test]
    async fn test_dispatch_malformed_json() {
        let (pending, _rxs) = make_pending(&[1]).await;
        let line = "this is not json at all";
        let result = dispatch_line(&pending, line).await;
        assert_eq!(result, DispatchResult::InvalidJson);
        assert!(pending.lock().await.contains_key(&1));
    }

    #[tokio::test]
    async fn test_dispatch_null_id_treated_as_notification() {
        let (pending, _rxs) = make_pending(&[1]).await;
        let line = r#"{"jsonrpc":"2.0","result":{},"id":null}"#;
        let result = dispatch_line(&pending, line).await;
        // null id → as_u64() returns None → treated as notification
        assert_eq!(result, DispatchResult::Notification);
    }

    // -----------------------------------------------------------------------
    // Integration tests using a real StdioAdapter with a mock echo server
    // -----------------------------------------------------------------------

    /// Create a StdioAdapter that launches a Python echo server.
    /// The server reads JSON-RPC requests from stdin and responds with
    /// a result containing the method name and request id.
    fn make_echo_adapter() -> StdioAdapter {
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        resp = {"jsonrpc": "2.0", "result": {"echo_method": req.get("method"), "echo_id": req.get("id")}, "id": req.get("id")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    except Exception:
        pass
"#;
        StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn test_stdio_concurrent_requests_return_correct_responses() {
        let mut adapter = make_echo_adapter();
        adapter.spawn_process().await.unwrap();

        // Give the Python process a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send two concurrent requests
        let adapter_ref = &adapter;
        let (r1, r2) = tokio::join!(
            adapter_ref.send_request("tools/list", None),
            adapter_ref.send_request("tools/call", Some(serde_json::json!({"name": "test"}))),
        );

        let v1 = r1.unwrap();
        let v2 = r2.unwrap();

        // Each response should echo back its OWN method — this is the exact
        // race condition bug: without ID matching, they could be swapped.
        assert_eq!(v1["echo_method"], "tools/list");
        assert_eq!(v2["echo_method"], "tools/call");

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_many_concurrent_requests() {
        let mut adapter = make_echo_adapter();
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Launch 10 concurrent requests, each with a unique method name
        let adapter_ref = &adapter;
        let mut handles = Vec::new();
        for i in 0..10 {
            let method = format!("method_{}", i);
            handles.push(async move {
                let result = adapter_ref.send_request(&method, None).await.unwrap();
                (method, result)
            });
        }

        let results = futures_util::future::join_all(handles).await;
        for (method, result) in &results {
            assert_eq!(
                result["echo_method"].as_str().unwrap(),
                method,
                "response mismatch for {}",
                method
            );
        }

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_sequential_requests_all_correct() {
        let mut adapter = make_echo_adapter();
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        for i in 0..5 {
            let method = format!("seq_{}", i);
            let result = adapter.send_request(&method, None).await.unwrap();
            assert_eq!(result["echo_method"].as_str().unwrap(), method);
        }

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_timeout_when_no_response() {
        // Use a server that never responds
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), "import time; time.sleep(120)".to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Use a short timeout by sending request (default is 30s, but we just
        // verify the error type)
        let result =
            tokio::time::timeout(Duration::from_secs(2), adapter.send_request("test", None)).await;

        // Either our wrapper times out or the inner 30s timeout fires —
        // either way, we don't hang forever.
        match result {
            Ok(Err(AdapterError::Timeout(_))) => {} // inner timeout
            Err(_) => {}                            // our 2s timeout
            other => panic!("unexpected result: {:?}", other),
        }

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_crash_error_is_short_user_message() {
        // A child that prints a diagnostic to stderr and exits without ever
        // responding on stdout. The crash error must now be the SHORT
        // user-facing banner — the stderr root cause is surfaced as its own
        // `[stderr]` rows in the Logs tab, NOT embedded in the error.
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'Error: OAuth keys file not found' >&2; exit 1".to_string(),
            ],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();

        // Either the stdin write fails (child already gone) or the response
        // channel closes — both paths now return the short crash banner.
        let result = adapter.send_request("initialize", None).await;
        match result {
            Err(AdapterError::ProcessCrashed(msg)) => {
                assert_eq!(
                    msg, CRASH_USER_MESSAGE,
                    "crash error should be the short user-facing banner, got: {msg}"
                );
                assert!(
                    !msg.contains("recent stderr:") && !msg.contains("OAuth keys file not found"),
                    "crash error must not embed child stderr, got: {msg}"
                );
            }
            other => panic!("expected ProcessCrashed, got: {other:?}"),
        }

        // The stderr root cause is still captured for the Logs tab buffer.
        let lines = adapter.stderr_lines().await;
        assert!(
            lines
                .iter()
                .any(|l| l.contains("OAuth keys file not found")),
            "child stderr should still be captured for the Logs tab, got: {lines:?}"
        );

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_crash_error_short_message_without_stderr() {
        // A child that exits abnormally but writes nothing to stderr must still
        // yield the same short crash banner (no embedded stderr section).
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 1".to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();

        let result = adapter.send_request("initialize", None).await;
        match result {
            Err(AdapterError::ProcessCrashed(msg)) => {
                assert_eq!(
                    msg, CRASH_USER_MESSAGE,
                    "crash error should be the short user-facing banner, got: {msg}"
                );
            }
            other => panic!("expected ProcessCrashed, got: {other:?}"),
        }

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_stderr_lines_tagged_for_logs() {
        // Captured stderr surfaced to the Logs tab must be tagged distinctly so
        // the desktop parser renders it apart from the relay's own logs.
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'hello from stderr' >&2; sleep 0.2".to_string(),
            ],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        let lines = adapter.stderr_lines().await;
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("WARN [stderr] ") && l.contains("hello from stderr")),
            "stderr lines should be tagged for the UI, got: {lines:?}"
        );

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_shutdown_drops_pending_requests() {
        let mut adapter = make_echo_adapter();
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Insert a pending request manually
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        adapter.pending_requests.lock().await.insert(9999, tx);

        // Shutdown should clear pending map
        adapter.shutdown().await.unwrap();

        // The receiver should get an error (sender dropped)
        assert!(rx.await.is_err());
        assert!(adapter.pending_requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_stdio_server_notification_doesnt_corrupt_requests() {
        // Server that sends a notification before each response
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        # Send a notification first (no id)
        notif = {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        sys.stdout.write(json.dumps(notif) + "\n")
        sys.stdout.flush()
        # Then send the actual response
        resp = {"jsonrpc": "2.0", "result": {"echo_method": req.get("method")}, "id": req.get("id")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    except Exception:
        pass
"#;
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The notification should be silently dropped, not returned as a response
        let result = adapter.send_request("tools/list", None).await.unwrap();
        assert_eq!(result["echo_method"], "tools/list");

        adapter.shutdown().await.unwrap();
    }

    // -----------------------------------------------------------------------
    // Auto-respawn supervisor tests
    // -----------------------------------------------------------------------

    /// Create a StdioAdapter around a minimal Python MCP server that answers
    /// the `initialize` handshake and `tools/list`, so the adapter reaches
    /// `Healthy` — the state the auto-respawn supervisor requires.
    fn make_mcp_server_adapter() -> StdioAdapter {
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    method = req.get("method")
    req_id = req.get("id")
    if req_id is None:
        continue
    if method == "initialize":
        resp = {
            "jsonrpc": "2.0",
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "respawn-test", "version": "0.1"},
            },
            "id": req_id,
        }
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "result": {"tools": []}, "id": req_id}
    else:
        resp = {"jsonrpc": "2.0", "result": {}, "id": req_id}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#;
        StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-u".to_string(), "-c".to_string(), script.to_string()],
            env: HashMap::new(),
            ..Default::default()
        })
    }

    /// Kill the adapter's current child with SIGKILL to simulate a crash.
    async fn kill_current_child(adapter: &StdioAdapter) {
        let pid = adapter
            .child
            .lock()
            .await
            .as_ref()
            .and_then(|c| c.id())
            .expect("adapter should have a running child");
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .await
            .expect("kill command should run");
        assert!(status.success(), "kill -9 {} failed", pid);
    }

    /// A crashed child must be respawned automatically: health returns to
    /// Healthy, a tools-changed tick fires after the post-restart handshake,
    /// and the respawned process serves requests again.
    #[tokio::test]
    async fn test_stdio_auto_respawn_after_crash() {
        let mut adapter = make_mcp_server_adapter();
        (&mut adapter as &mut dyn McpAdapter)
            .initialize()
            .await
            .unwrap();
        assert_eq!(adapter.health(), HealthStatus::Healthy);

        // Subscribe BEFORE the crash so the post-restart tick is captured.
        let mut rx = (&adapter as &dyn McpAdapter)
            .subscribe_tools_changed()
            .unwrap();

        kill_current_child(&adapter).await;

        // First crash → 1s backoff, then respawn + re-handshake + tick. The
        // generous timeout absorbs CI scheduling noise.
        let tick = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await;
        assert!(
            matches!(tick, Ok(Ok(()))),
            "expected post-respawn tools-changed tick, got {tick:?}"
        );
        assert_eq!(adapter.health(), HealthStatus::Healthy);

        // The respawned process answers requests.
        let result = adapter.send_request("tools/list", None).await.unwrap();
        assert!(result.get("tools").is_some());

        adapter.shutdown().await.unwrap();
    }

    /// An intentional shutdown must NOT trigger a respawn, and a later
    /// re-initialize (the manual-restart path) must re-arm the supervisor.
    #[tokio::test]
    async fn test_stdio_shutdown_suppresses_respawn_and_reinit_rearms() {
        let mut adapter = make_mcp_server_adapter();
        (&mut adapter as &mut dyn McpAdapter)
            .initialize()
            .await
            .unwrap();
        adapter.shutdown().await.unwrap();

        // Give an (erroneous) supervisor time to run its 1s first backoff.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(adapter.health(), HealthStatus::Stopped);
        assert!(
            adapter.child.lock().await.is_none(),
            "no child may be respawned after an intentional shutdown"
        );
        assert!(
            !adapter.respawning.load(Ordering::SeqCst),
            "no supervisor may be running after an intentional shutdown"
        );

        // Manual-restart path: initialize() again must clear the shutdown
        // flag and bring the adapter back to Healthy.
        (&mut adapter as &mut dyn McpAdapter)
            .initialize()
            .await
            .unwrap();
        assert_eq!(adapter.health(), HealthStatus::Healthy);
        assert!(!adapter.shutdown_requested.load(Ordering::SeqCst));

        adapter.shutdown().await.unwrap();
    }

    /// A child that exits before ever completing the handshake (initialize
    /// fails) must not leave a supervisor respawning an orphaned process.
    #[tokio::test]
    async fn test_stdio_no_respawn_when_never_healthy() {
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "exit 1".to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        let result = (&mut adapter as &mut dyn McpAdapter).initialize().await;
        assert!(result.is_err(), "initialize should fail for a dying child");

        // The EOF hook fires, but the health gate must stop the supervisor.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !adapter.respawning.load(Ordering::SeqCst),
            "supervisor must not run for a child that never became healthy"
        );

        adapter.shutdown().await.unwrap();
    }

    // -----------------------------------------------------------------------
    // tools-changed broadcast tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_subscribe_tools_changed_returns_some() {
        let adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), "pass".to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        assert!(
            (&adapter as &dyn McpAdapter)
                .subscribe_tools_changed()
                .is_some(),
            "stdio adapter should expose a tools-changed receiver"
        );
    }

    #[tokio::test]
    async fn test_stdio_emits_tick_on_list_changed_notification() {
        // Server that, after each request, prints a tools/list_changed
        // notification *before* its response. Subscribing before triggering
        // the request must yield at least one tick on the broadcast channel.
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        notif = {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        sys.stdout.write(json.dumps(notif) + "\n")
        sys.stdout.flush()
        resp = {"jsonrpc": "2.0", "result": {"echo_method": req.get("method")}, "id": req.get("id")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    except Exception:
        pass
"#;
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut rx = (&adapter as &dyn McpAdapter)
            .subscribe_tools_changed()
            .expect("stdio adapter exposes a tools-changed receiver");

        // Trigger the server to emit the notification.
        let _ = adapter.send_request("tools/list", None).await.unwrap();

        let recv = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        match recv {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("broadcast recv error: {:?}", e),
            Err(_) => panic!("did not receive tools-changed tick within 1s"),
        }

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_unrelated_notification_does_not_emit_tick() {
        // Server that emits an unrelated notification before its response.
        // The tools-changed subscriber must NOT receive a tick.
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        notif = {"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hi"}}
        sys.stdout.write(json.dumps(notif) + "\n")
        sys.stdout.flush()
        resp = {"jsonrpc": "2.0", "result": {"echo_method": req.get("method")}, "id": req.get("id")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    except Exception:
        pass
"#;
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            ..Default::default()
        });
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut rx = (&adapter as &dyn McpAdapter)
            .subscribe_tools_changed()
            .expect("stdio adapter exposes a tools-changed receiver");

        let _ = adapter.send_request("tools/list", None).await.unwrap();

        // Give the stdout reader a moment to process the notification line.
        tokio::time::sleep(Duration::from_millis(100)).await;

        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            other => panic!("expected no tick, got {:?}", other),
        }

        adapter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_stdio_sends_initialized_notification_after_handshake() {
        // Python script that records every received stdin line to a file
        // (path passed via the RECORD_PATH env var) and responds to the
        // initialize and tools/list requests with minimal valid results. The
        // `server/discover` probe (T9) is answered with an empty result, so the
        // adapter detects a legacy upstream and runs the full handshake.
        let script = r#"
import sys, json, os
record_path = os.environ["RECORD_PATH"]
with open(record_path, "w") as rec:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rec.write(line + "\n")
        rec.flush()
        try:
            req = json.loads(line)
        except Exception:
            continue
        method = req.get("method")
        req_id = req.get("id")
        if req_id is None:
            # Notifications get no response.
            continue
        if method == "initialize":
            resp = {
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "test", "version": "0.1"},
                },
                "id": req_id,
            }
        elif method == "tools/list":
            resp = {"jsonrpc": "2.0", "result": {"tools": []}, "id": req_id}
        else:
            resp = {"jsonrpc": "2.0", "result": {}, "id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;
        let record_path = std::env::temp_dir().join(format!(
            "endara-stdio-init-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut env = HashMap::new();
        env.insert(
            "RECORD_PATH".to_string(),
            record_path.to_string_lossy().into_owned(),
        );
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-u".to_string(), "-c".to_string(), script.to_string()],
            env,
            ..Default::default()
        });

        (&mut adapter as &mut dyn McpAdapter)
            .initialize()
            .await
            .unwrap();
        let _ = adapter.list_tools().await.unwrap();

        // Give the Python script a moment to flush before we read the file.
        tokio::time::sleep(Duration::from_millis(100)).await;
        adapter.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let recorded = std::fs::read_to_string(&record_path)
            .expect("record file should exist after handshake + list_tools");
        let _ = std::fs::remove_file(&record_path);
        let frames: Vec<Value> = recorded
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each recorded line is valid JSON"))
            .collect();

        assert!(
            frames.len() >= 4,
            "expected at least 4 frames, got {}: {:?}",
            frames.len(),
            frames
        );
        // The discover-first probe (T9) precedes the legacy handshake. It is the
        // only legacy-path frame that carries `_meta` clientInfo (it is sent
        // before the dialect is known, so identity is attached explicitly).
        assert_eq!(frames[0]["method"].as_str(), Some("server/discover"));
        assert_eq!(
            frames[0]["params"]["_meta"][crate::protocol::META_CLIENT_INFO_KEY]["name"].as_str(),
            Some("endara-relay"),
            "discover probe must carry _meta clientInfo, got: {:?}",
            frames[0]
        );
        assert_eq!(frames[1]["method"].as_str(), Some("initialize"));
        assert!(
            frames[1].get("id").and_then(|v| v.as_u64()).is_some(),
            "initialize frame must carry a numeric id"
        );
        // Legacy upstream: the relay must NOT inject `_meta` clientInfo on the
        // handshake/tool frames — only the 2026 stateless path does.
        assert!(
            frames[1]["params"].get("_meta").is_none(),
            "legacy initialize frame must not carry _meta, got: {:?}",
            frames[1]
        );
        assert_eq!(
            frames[2]["method"].as_str(),
            Some("notifications/initialized")
        );
        assert!(
            frames[2].get("id").is_none(),
            "notifications/initialized frame must not have an id field, got: {:?}",
            frames[2]
        );
        assert_eq!(frames[3]["method"].as_str(), Some("tools/list"));
        assert!(
            frames[3]["params"].get("_meta").is_none(),
            "legacy tools/list frame must not carry _meta, got: {:?}",
            frames[3]
        );
    }

    /// Silent-drop upstream over stdio: the legacy server never answers the
    /// `server/discover` probe (T9) but responds normally to `initialize`. The
    /// probe must fail fast via [`DISCOVER_PROBE_TIMEOUT`] and the relay must
    /// fall back to the legacy handshake well within the bound — NOT stall on the
    /// 30s per-request default. This is the MCP `2026-07-28` "no response within
    /// a reasonable timeout → legacy" rule.
    #[tokio::test]
    async fn test_stdio_silent_discover_falls_back_to_initialize_fast() {
        // Python mock: record every received line, answer `initialize` and
        // `tools/list`, but SILENTLY DROP `server/discover` (no response).
        let script = r#"
import sys, json, os
record_path = os.environ["RECORD_PATH"]
with open(record_path, "w") as rec:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rec.write(line + "\n")
        rec.flush()
        try:
            req = json.loads(line)
        except Exception:
            continue
        method = req.get("method")
        req_id = req.get("id")
        if req_id is None:
            continue
        if method == "server/discover":
            # Silently drop the unknown probe — emit no response at all.
            continue
        if method == "initialize":
            resp = {
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "test", "version": "0.1"},
                },
                "id": req_id,
            }
        elif method == "tools/list":
            resp = {"jsonrpc": "2.0", "result": {"tools": []}, "id": req_id}
        else:
            resp = {"jsonrpc": "2.0", "result": {}, "id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;
        let record_path = std::env::temp_dir().join(format!(
            "endara-stdio-silent-discover-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut env = HashMap::new();
        env.insert(
            "RECORD_PATH".to_string(),
            record_path.to_string_lossy().into_owned(),
        );
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-u".to_string(), "-c".to_string(), script.to_string()],
            env,
            ..Default::default()
        });

        let start = Instant::now();
        (&mut adapter as &mut dyn McpAdapter)
            .initialize()
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // The probe is bounded by DISCOVER_PROBE_TIMEOUT, then the legacy
        // handshake runs. The whole thing must complete far below the 30s
        // per-request default — well under 10s leaves generous CI headroom while
        // still proving we did not stall on the 30s path.
        assert!(
            elapsed < Duration::from_secs(10),
            "initialize must fall back fast after a silent discover probe, took {:?}",
            elapsed
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        adapter.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let recorded = std::fs::read_to_string(&record_path)
            .expect("record file should exist after handshake");
        let _ = std::fs::remove_file(&record_path);
        let frames: Vec<Value> = recorded
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each recorded line is valid JSON"))
            .collect();

        let methods: Vec<&str> = frames.iter().filter_map(|f| f["method"].as_str()).collect();
        // The probe was still sent (then dropped), and the relay fell back to the
        // legacy handshake.
        assert!(
            methods.contains(&"server/discover"),
            "discover probe must be sent, got {methods:?}"
        );
        assert!(
            methods.contains(&"initialize"),
            "must fall back to legacy initialize, got {methods:?}"
        );
        assert!(
            methods.contains(&"notifications/initialized"),
            "legacy fallback must send notifications/initialized, got {methods:?}"
        );
    }

    #[test]
    fn test_inject_client_info_creates_and_preserves_params() {
        // None params → a fresh object carrying only `_meta` clientInfo.
        let injected = StdioAdapter::inject_client_info(None).unwrap();
        let ci = &injected["_meta"][crate::protocol::META_CLIENT_INFO_KEY];
        assert_eq!(ci["name"], "endara-relay");
        assert!(ci["version"].is_string());

        // Existing fields are preserved; `_meta` clientInfo is added.
        let injected =
            StdioAdapter::inject_client_info(Some(json!({"name": "echo", "arguments": {}})))
                .unwrap();
        assert_eq!(injected["name"], "echo");
        assert_eq!(
            injected["_meta"][crate::protocol::META_CLIENT_INFO_KEY]["name"],
            "endara-relay"
        );

        // A pre-existing OBJECT `_meta` with sibling keys (e.g. W3C Trace
        // Context) is preserved; clientInfo is added alongside the siblings.
        let injected = StdioAdapter::inject_client_info(Some(json!({
            "name": "echo",
            "_meta": {"traceparent": "tp", "tracestate": "ts"}
        })))
        .unwrap();
        assert_eq!(injected["_meta"]["traceparent"], "tp");
        assert_eq!(injected["_meta"]["tracestate"], "ts");
        assert_eq!(
            injected["_meta"][crate::protocol::META_CLIENT_INFO_KEY]["name"],
            "endara-relay"
        );

        // A pre-existing NON-OBJECT `_meta` (here a String) must NOT panic:
        // it is normalized to an object and clientInfo is still injected.
        let injected = StdioAdapter::inject_client_info(Some(
            json!({"name": "echo", "_meta": "not-an-object"}),
        ))
        .unwrap();
        assert!(injected["_meta"].is_object());
        assert_eq!(
            injected["_meta"][crate::protocol::META_CLIENT_INFO_KEY]["name"],
            "endara-relay"
        );
    }

    /// 2026 upstream over stdio: the `server/discover` probe detects
    /// `2026-07-28`, so the adapter skips `initialize`/
    /// `notifications/initialized` entirely, and every subsequent request
    /// carries `_meta` clientInfo (stdio has no HTTP headers, so identity travels
    /// in `params._meta`).
    #[tokio::test]
    async fn test_stdio_2026_path_skips_handshake_and_injects_meta() {
        // Python mock: answer `server/discover` with a 2026 result + serverInfo;
        // respond to `tools/list`; record every received line.
        let script = r#"
import sys, json, os
record_path = os.environ["RECORD_PATH"]
with open(record_path, "w") as rec:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        rec.write(line + "\n")
        rec.flush()
        try:
            req = json.loads(line)
        except Exception:
            continue
        method = req.get("method")
        req_id = req.get("id")
        if req_id is None:
            continue
        if method == "server/discover":
            resp = {
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2026-07-28",
                    "capabilities": {},
                    "serverInfo": {"name": "test-2026", "version": "1.0"},
                },
                "id": req_id,
            }
        elif method == "tools/list":
            resp = {"jsonrpc": "2.0", "result": {"tools": []}, "id": req_id}
        else:
            resp = {"jsonrpc": "2.0", "result": {}, "id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;
        let record_path = std::env::temp_dir().join(format!(
            "endara-stdio-2026-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut env = HashMap::new();
        env.insert(
            "RECORD_PATH".to_string(),
            record_path.to_string_lossy().into_owned(),
        );
        let mut adapter = StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-u".to_string(), "-c".to_string(), script.to_string()],
            env,
            ..Default::default()
        });

        (&mut adapter as &mut dyn McpAdapter)
            .initialize()
            .await
            .unwrap();
        assert!(
            adapter.upstream_dialect().await.is_2026(),
            "upstream should be detected as 2026"
        );
        let _ = adapter.list_tools().await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        adapter.shutdown().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let recorded = std::fs::read_to_string(&record_path)
            .expect("record file should exist after 2026 discover + list_tools");
        let _ = std::fs::remove_file(&record_path);
        let frames: Vec<Value> = recorded
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each recorded line is valid JSON"))
            .collect();

        let methods: Vec<&str> = frames.iter().filter_map(|f| f["method"].as_str()).collect();
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
        // Every recorded frame on the 2026 path carries `_meta` clientInfo.
        for f in &frames {
            assert_eq!(
                f["params"]["_meta"][crate::protocol::META_CLIENT_INFO_KEY]["name"].as_str(),
                Some("endara-relay"),
                "every 2026 frame must carry _meta clientInfo, got: {f:?}"
            );
        }
    }

    /// End-to-end sanity check that `request_uid` flows from the surrounding
    /// `request` tracing span into the published [`ToolCallEvent::Started`]
    /// event. Uses a non-spawned `StdioAdapter` so `send_request` returns
    /// `AdapterError::NotInitialized` immediately — the `Started` event still
    /// publishes before the network attempt, and the `Failed` event publishes
    /// on the resulting `Err`.
    ///
    /// `#[test]` (not `#[tokio::test]`) because we install the capture layer
    /// via `with_default(...)` and drive an inner current-thread runtime.
    #[test]
    #[serial_test::serial(tracing)]
    fn call_tool_publishes_request_uid_from_request_span() {
        crate::test_tracing::init_permissive_tracing();
        use crate::events::{SpanFieldCaptureLayer, ToolCallEvent, ToolCallEventBus};
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;

        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let bus = ToolCallEventBus::with_default_capacity();
                let adapter = StdioAdapter::new(StdioConfig {
                    command: "nonexistent-binary-for-span-test".to_string(),
                    args: vec![],
                    env: HashMap::new(),
                    ..Default::default()
                });
                adapter.set_event_bus(bus.clone());
                let mut rx = bus.subscribe();

                let uid_str = "uid-99".to_string();
                let span =
                    tracing::info_span!("request", method = "tools/call", request_uid = %uid_str);
                let result = async { adapter.call_tool("nope", serde_json::json!({})).await }
                    .instrument(span)
                    .await;
                assert!(
                    result.is_err(),
                    "expected NotInitialized error, got {result:?}"
                );

                let started = rx.try_recv().expect("started event must be buffered");
                match started {
                    ToolCallEvent::Started { request_uid, .. } => {
                        assert_eq!(request_uid.as_deref(), Some("uid-99"));
                    }
                    other => panic!("expected Started event, got {other:?}"),
                }
                let failed = rx.try_recv().expect("failed event must be buffered");
                match failed {
                    ToolCallEvent::Failed { .. } => {}
                    other => panic!("expected Failed event, got {other:?}"),
                }
            });
        });
    }

    /// Create a StdioAdapter whose mock server answers `tools/call` with a
    /// tool-level error envelope (`isError: true`) when the tool is named
    /// `fail`, and a plain success envelope otherwise.
    fn make_iserror_adapter() -> StdioAdapter {
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
        if req.get("method") == "tools/call" and req.get("params", {}).get("name") == "fail":
            result = {"content": [{"type": "text", "text": "invalid_grant"}], "isError": True}
        else:
            result = {"content": [{"type": "text", "text": "all good"}]}
        resp = {"jsonrpc": "2.0", "result": result, "id": req.get("id")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    except Exception:
        pass
"#;
        StdioAdapter::new(StdioConfig {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            ..Default::default()
        })
    }

    /// A transport-level `Ok` whose envelope carries `isError: true` must be
    /// surfaced as a FAILED call: the event bus receives `Started` → `Failed`
    /// (not `Completed`) with the captured message, matching the registry's
    /// durable capture. The envelope itself is still forwarded unchanged.
    #[tokio::test]
    async fn call_tool_iserror_envelope_emits_failed_event() {
        use crate::events::{ToolCallEvent, ToolCallEventBus};

        let mut adapter = make_iserror_adapter();
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bus = ToolCallEventBus::with_default_capacity();
        adapter.set_event_bus(bus.clone());
        let mut rx = bus.subscribe();

        let value = adapter
            .call_tool("fail", serde_json::json!({}))
            .await
            .expect("isError envelope is still a transport-level Ok");
        assert_eq!(value["isError"], true, "envelope forwarded unchanged");

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
        adapter.shutdown().await.unwrap();
    }

    /// A plain success envelope (`isError` absent) keeps the pre-existing
    /// behavior: a `Completed` event with `status=ok`.
    #[tokio::test]
    async fn call_tool_success_envelope_emits_completed_event() {
        use crate::events::{ToolCallEvent, ToolCallEventBus};

        let mut adapter = make_iserror_adapter();
        adapter.spawn_process().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bus = ToolCallEventBus::with_default_capacity();
        adapter.set_event_bus(bus.clone());
        let mut rx = bus.subscribe();

        adapter
            .call_tool("echo", serde_json::json!({}))
            .await
            .expect("success envelope");

        match rx.try_recv().expect("started event must be buffered") {
            ToolCallEvent::Started { .. } => {}
            other => panic!("expected Started, got {other:?}"),
        }
        match rx.try_recv().expect("terminal event must be buffered") {
            ToolCallEvent::Completed { status, .. } => assert_eq!(status, "ok"),
            other => panic!("expected Completed, got {other:?}"),
        }
        adapter.shutdown().await.unwrap();
    }
}
