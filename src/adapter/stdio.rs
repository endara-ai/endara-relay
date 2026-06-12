use super::server_name::{sanitize_server_name, ServerNameError};
use super::server_type_resolution::{effective_server_type, strip_mcp_server_suffix};
use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::container_stats::{self, ContainerStats, StatsSlot};
use crate::events::{
    annotations_from_value, current_request_context, ToolCallEvent, ToolCallEventBus,
};
use crate::jsonrpc::{self, JsonRpcResponse};
use crate::shell_env;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn, Instrument};

/// Default OCI image used for containerized stdio servers when the endpoint
/// does not specify a `container_image`.
pub const DEFAULT_CONTAINER_IMAGE: &str = "ghcr.io/endara-ai/mcp-runner:latest";

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
#[allow(dead_code)] // Used by try_respawn, kept for respawn support
struct CrashTracker {
    timestamps: Vec<Instant>,
    consecutive_crashes: u32,
}

impl CrashTracker {
    fn new() -> Self {
        Self {
            timestamps: Vec::new(),
            consecutive_crashes: 0,
        }
    }

    /// Record a crash and return whether the adapter should be marked unhealthy.
    #[allow(dead_code)] // Used by try_respawn
    fn record_crash(&mut self) -> bool {
        let now = Instant::now();
        self.consecutive_crashes += 1;
        self.timestamps.push(now);

        // Remove crashes older than 60 seconds
        let cutoff = now - Duration::from_secs(60);
        self.timestamps.retain(|t| *t >= cutoff);

        // If 3+ crashes in 60 seconds, mark unhealthy
        self.timestamps.len() >= 3
    }

    /// Calculate backoff duration based on consecutive crashes.
    #[allow(dead_code)] // Used by try_respawn
    fn backoff_duration(&self) -> Duration {
        let secs = match self.consecutive_crashes {
            0 => 1,
            1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => 60,
        };
        Duration::from_secs(secs)
    }

    fn reset(&mut self) {
        self.consecutive_crashes = 0;
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
pub struct StdioAdapter {
    config: StdioConfig,
    child: Arc<Mutex<Option<Child>>>,
    stdin_writer: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    pending_requests: PendingRequests,
    stderr_buffer: Arc<RwLock<RingBuffer>>,
    health: Arc<RwLock<HealthStatus>>,
    request_id: AtomicU64,
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
            request_id: AtomicU64::new(1),
            crash_tracker: Arc::new(Mutex::new(CrashTracker::new())),
            server_type: Arc::new(RwLock::new(None)),
            upstream_server_name: Arc::new(RwLock::new(None)),
            tools_changed_tx,
            span,
            event_bus: Arc::new(OnceLock::new()),
            tool_annotations_cache: Arc::new(RwLock::new(HashMap::new())),
            container: Arc::new(Mutex::new(None)),
            container_stats: Arc::new(std::sync::RwLock::new(None)),
            stats_poller_handle: Arc::new(Mutex::new(None)),
            _stderr_handle: Arc::new(Mutex::new(None)),
            _stdout_handle: Arc::new(Mutex::new(None)),
        }
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Spawn the child process and set up I/O pipes.
    async fn spawn_process(&self) -> Result<(), AdapterError> {
        *self.health.write().await = HealthStatus::Starting;

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
        // TODO: emit tick after post-restart handshake when auto-restart is added
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
            // waiters immediately get a RecvError instead of hanging until timeout.
            let mut map = pending.lock().await;
            if !map.is_empty() {
                debug!(
                    count = map.len(),
                    "stdout closed, dropping pending requests"
                );
                map.clear();
            }
        });

        // Set up stderr ring buffer reader
        let stderr_buf = self.stderr_buffer.clone();
        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(stderr_line = %line, "MCP server stderr");
                stderr_buf.write().await.push(line);
            }
        });

        *self.child.lock().await = Some(child);
        *self.stdin_writer.lock().await = Some(stdin);
        *self._stdout_handle.lock().await = Some(stdout_handle);
        *self._stderr_handle.lock().await = Some(stderr_handle);

        info!(command = %self.config.command, "MCP server process spawned");
        Ok(())
    }

    /// Send a JSON-RPC request and wait for the response.
    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AdapterError> {
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
                return Err(AdapterError::ProcessCrashed(format!(
                    "stdin write failed: {}",
                    e
                )));
            }
            if let Err(e) = writer.flush().await {
                self.pending_requests.lock().await.remove(&id);
                return Err(AdapterError::ProcessCrashed(format!(
                    "stdin flush failed: {}",
                    e
                )));
            }
        }

        // Await the response with timeout (lock is NOT held during await)
        let response_line = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(line)) => line,
            Ok(Err(_)) => {
                // Sender was dropped (stdout reader shut down)
                self.pending_requests.lock().await.remove(&id);
                return Err(AdapterError::ProcessCrashed("stdout channel closed".into()));
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
        let notification = jsonrpc::new_notification(method, params);
        let mut line = serde_json::to_string(&notification)?;
        line.push('\n');

        let mut writer_guard = self.stdin_writer.lock().await;
        let writer = writer_guard.as_mut().ok_or(AdapterError::NotInitialized)?;
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            return Err(AdapterError::ProcessCrashed(format!(
                "stdin write failed: {}",
                e
            )));
        }
        if let Err(e) = writer.flush().await {
            return Err(AdapterError::ProcessCrashed(format!(
                "stdin flush failed: {}",
                e
            )));
        }
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
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "endara-relay",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.send_request("initialize", Some(params)).await?;

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
            self.span
                .record("server_type", tracing::field::display(name));
        }
        *self.server_type.write().await = effective;
        *self.upstream_server_name.write().await = Some(upstream_stripped);

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
}

#[async_trait]
impl McpAdapter for StdioAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        async {
            self.spawn_process().await?;
            self.mcp_initialize().await?;
            *self.health.write().await = HealthStatus::Healthy;
            self.crash_tracker.lock().await.reset();
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

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
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
                    jsonrpc_id: span_ctx.jsonrpc_id.clone(),
                    ts: iso8601_now(),
                    endpoint: self.config.endpoint_name.clone(),
                    transport: "stdio".into(),
                    server_type: self.server_type.read().await.clone(),
                    server_name: self.upstream_server_name.read().await.clone(),
                    profile: span_ctx.profile.clone(),
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
                        jsonrpc_id: span_ctx.jsonrpc_id.clone(),
                        ts,
                        duration_ms: duration_ms_u64,
                        status: "ok".into(),
                    }),
                    Err(e) => bus.send(ToolCallEvent::Failed {
                        request_id,
                        jsonrpc_id: span_ctx.jsonrpc_id.clone(),
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
        self.stderr_buffer
            .read()
            .await
            .lines()
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        async {
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

/// Attempt to respawn after a crash with exponential backoff.
/// Returns Err if the adapter should be marked permanently unhealthy.
#[allow(dead_code)] // Kept for future respawn support
pub async fn try_respawn(adapter: &mut StdioAdapter) -> Result<(), AdapterError> {
    let should_stop = {
        let mut tracker = adapter.crash_tracker.lock().await;
        let unhealthy = tracker.record_crash();
        if unhealthy {
            true
        } else {
            let backoff = tracker.backoff_duration();
            info!(
                backoff_secs = backoff.as_secs(),
                "backing off before respawn"
            );
            drop(tracker);
            tokio::time::sleep(backoff).await;
            false
        }
    };

    if should_stop {
        let reason = "3+ crashes in 60 seconds".to_string();
        *adapter.health.write().await = HealthStatus::Unhealthy(reason.clone());
        error!("adapter marked unhealthy: {}", reason);
        return Err(AdapterError::ProcessCrashed(reason));
    }

    adapter.initialize().await
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
        assert!(!tracker.record_crash()); // 1st crash
        assert!(!tracker.record_crash()); // 2nd crash
        assert!(tracker.record_crash()); // 3rd crash → unhealthy
    }

    #[test]
    fn test_crash_tracker_backoff_increases() {
        let mut tracker = CrashTracker::new();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(1));
        tracker.record_crash();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(1));
        tracker.record_crash();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(2));
    }

    #[test]
    fn test_crash_tracker_reset() {
        let mut tracker = CrashTracker::new();
        tracker.record_crash();
        tracker.record_crash();
        tracker.reset();
        assert_eq!(tracker.backoff_duration(), Duration::from_secs(1));
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
        // initialize and tools/list requests with minimal valid results.
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
            frames.len() >= 3,
            "expected at least 3 frames, got {}: {:?}",
            frames.len(),
            frames
        );
        assert_eq!(frames[0]["method"].as_str(), Some("initialize"));
        assert!(
            frames[0].get("id").and_then(|v| v.as_u64()).is_some(),
            "initialize frame must carry a numeric id"
        );
        assert_eq!(
            frames[1]["method"].as_str(),
            Some("notifications/initialized")
        );
        assert!(
            frames[1].get("id").is_none(),
            "notifications/initialized frame must not have an id field, got: {:?}",
            frames[1]
        );
        assert_eq!(frames[2]["method"].as_str(), Some("tools/list"));
    }

    /// End-to-end sanity check that `jsonrpc_id` flows from the surrounding
    /// `request` tracing span into the published [`ToolCallEvent::Started`]
    /// and [`ToolCallEvent::Failed`] events. Uses a non-spawned `StdioAdapter`
    /// so `send_request` returns `AdapterError::NotInitialized` immediately —
    /// the `Started` event still publishes before the network attempt, and
    /// the `Failed` event publishes on the resulting `Err`.
    ///
    /// `#[test]` (not `#[tokio::test]`) because we install the capture layer
    /// via `with_default(...)` and drive an inner current-thread runtime.
    #[test]
    fn call_tool_publishes_jsonrpc_id_from_request_span() {
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

                let id_str = "99".to_string();
                let span = tracing::info_span!("request", method = "tools/call", id = %id_str);
                let result = async { adapter.call_tool("nope", serde_json::json!({})).await }
                    .instrument(span)
                    .await;
                assert!(
                    result.is_err(),
                    "expected NotInitialized error, got {result:?}"
                );

                let started = rx.try_recv().expect("started event must be buffered");
                match started {
                    ToolCallEvent::Started { jsonrpc_id, .. } => {
                        assert_eq!(jsonrpc_id.as_deref(), Some("99"));
                    }
                    other => panic!("expected Started event, got {other:?}"),
                }
                let failed = rx.try_recv().expect("failed event must be buffered");
                match failed {
                    ToolCallEvent::Failed { jsonrpc_id, .. } => {
                        assert_eq!(jsonrpc_id.as_deref(), Some("99"));
                    }
                    other => panic!("expected Failed event, got {other:?}"),
                }
            });
        });
    }
}
