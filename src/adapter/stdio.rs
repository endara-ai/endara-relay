use super::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::jsonrpc::{self, JsonRpcResponse};
use crate::prefix;
use crate::shell_env;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Configuration for spawning a STDIO MCP server.
#[derive(Debug, Clone)]
pub struct StdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
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

/// STDIO MCP adapter — spawns a child process and communicates via stdin/stdout.
pub struct StdioAdapter {
    config: StdioConfig,
    child: Arc<Mutex<Option<Child>>>,
    stdin_writer: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    stdout_lines: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<String>>>>,
    stderr_buffer: Arc<RwLock<RingBuffer>>,
    health: Arc<RwLock<HealthStatus>>,
    request_id: AtomicU64,
    crash_tracker: Arc<Mutex<CrashTracker>>,
    /// Sanitized server name from the MCP initialize response.
    server_type: Arc<RwLock<Option<String>>>,
    // Background task handles
    _stderr_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    _stdout_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl StdioAdapter {
    /// Create a new StdioAdapter with the given configuration.
    pub fn new(config: StdioConfig) -> Self {
        Self {
            config,
            child: Arc::new(Mutex::new(None)),
            stdin_writer: Arc::new(Mutex::new(None)),
            stdout_lines: Arc::new(Mutex::new(None)),
            stderr_buffer: Arc::new(RwLock::new(RingBuffer::new(1000))),
            health: Arc::new(RwLock::new(HealthStatus::Stopped)),
            request_id: AtomicU64::new(1),
            crash_tracker: Arc::new(Mutex::new(CrashTracker::new())),
            server_type: Arc::new(RwLock::new(None)),
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

        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Inject the user's login-shell PATH so that commands installed via
        // nvm, Homebrew, pyenv, etc. are discoverable even when the relay
        // runs as a Tauri sidecar with a minimal inherited environment.
        if let Some(shell_path) = shell_env::resolve_shell_path() {
            if !self.config.env.contains_key("PATH") {
                cmd.env("PATH", shell_path);
            }
        }

        // User-specified env vars always win (applied after shell PATH).
        cmd.envs(&self.config.env);

        let mut child = cmd.spawn().map_err(|e| {
            AdapterError::ProcessSpawnFailed(format!("{}: {}", self.config.command, e))
        })?;

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

        // Set up stdout line reader via channel
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(256);
        let stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
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
        *self.stdout_lines.lock().await = Some(rx);
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

        // Write to stdin
        {
            let mut writer_guard = self.stdin_writer.lock().await;
            let writer = writer_guard.as_mut().ok_or(AdapterError::NotInitialized)?;
            writer
                .write_all(line.as_bytes())
                .await
                .map_err(|e| AdapterError::ProcessCrashed(format!("stdin write failed: {}", e)))?;
            writer
                .flush()
                .await
                .map_err(|e| AdapterError::ProcessCrashed(format!("stdin flush failed: {}", e)))?;
        }

        // Read response from stdout with timeout
        let response_line = {
            let mut rx_guard = self.stdout_lines.lock().await;
            let rx = rx_guard.as_mut().ok_or(AdapterError::NotInitialized)?;
            tokio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .map_err(|_| AdapterError::Timeout(30))?
                .ok_or_else(|| AdapterError::ProcessCrashed("stdout channel closed".into()))?
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

    /// Perform the MCP initialize handshake.
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

        // Capture serverInfo.name from the initialize response
        if let Some(name) = result
            .get("serverInfo")
            .and_then(|si| si.get("name"))
            .and_then(|n| n.as_str())
        {
            let sanitized = prefix::sanitize_name(name);
            info!(raw_name = %name, sanitized = ?sanitized, "MCP server reported serverInfo.name");
            *self.server_type.write().await = sanitized;
        }

        info!("MCP initialize handshake complete");
        Ok(())
    }
}

#[async_trait]
impl McpAdapter for StdioAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        self.spawn_process().await?;
        self.mcp_initialize().await?;
        *self.health.write().await = HealthStatus::Healthy;
        self.crash_tracker.lock().await.reset();
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
        self.send_request("tools/call", Some(params)).await
    }

    fn health(&self) -> HealthStatus {
        // Use try_read to avoid blocking; fall back to Starting
        match self.health.try_read() {
            Ok(h) => h.clone(),
            Err(_) => HealthStatus::Starting,
        }
    }

    fn server_type(&self) -> Option<String> {
        self.server_type.try_read().ok().and_then(|g| g.clone())
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
        *self.health.write().await = HealthStatus::Stopped;

        // Try graceful close via stdin
        if let Some(stdin) = self.stdin_writer.lock().await.take() {
            drop(stdin);
        }

        // Drop stdout receiver
        self.stdout_lines.lock().await.take();

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
}
