pub mod http;
pub mod oauth;
pub mod server_name;
pub mod server_type_resolution;
pub mod sse;
pub mod stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Health status of an adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HealthStatus {
    Healthy,
    Unhealthy(String),
    Starting,
    Stopped,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthStatus::Healthy => write!(f, "healthy"),
            HealthStatus::Unhealthy(reason) => write!(f, "unhealthy: {}", reason),
            HealthStatus::Starting => write!(f, "starting"),
            HealthStatus::Stopped => write!(f, "stopped"),
        }
    }
}

/// Information about a tool exposed by an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

/// Max character count retained from `HttpError.body` when formatting an
/// `AdapterError` for display. Upstream MCP servers occasionally return
/// multi-KB HTML error pages or stack traces; including those verbatim
/// in `Failed` events would leak noise into the management UI / logs.
pub const HTTP_ERROR_BODY_MAX_CHARS: usize = 200;

/// Return at most `max_chars` characters from `s`, appending `"…"` if the
/// input was longer. Counts by `char` (not byte) so the result is always a
/// valid UTF-8 boundary even for multi-byte input.
pub fn truncate_for_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// Errors that can occur in adapter operations.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("failed to spawn process: {0}")]
    ProcessSpawnFailed(String),

    #[error("process crashed: {0}")]
    ProcessCrashed(String),

    #[error("operation timed out after {0}s")]
    Timeout(u64),

    #[error("JSON-RPC error {code}: {message}")]
    JsonRpcError {
        code: i64,
        message: String,
        data: Option<Value>,
    },

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("adapter not initialized")]
    NotInitialized,

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error(
        "HTTP error {status}: {}",
        truncate_for_display(body, HTTP_ERROR_BODY_MAX_CHARS)
    )]
    HttpError { status: u16, body: String },

    #[error("Authentication required for endpoint '{endpoint}': {message}")]
    AuthenticationRequired { endpoint: String, message: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Trait for MCP server adapters.
///
/// Each adapter manages the lifecycle of a connection to an MCP server
/// and provides methods to interact with it.
#[async_trait]
pub trait McpAdapter: Send + Sync {
    /// Initialize the adapter and perform the MCP handshake.
    async fn initialize(&mut self) -> Result<(), AdapterError>;

    /// List the tools available from the MCP server.
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError>;

    /// Call a tool on the MCP server.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError>;

    /// Get the current health status.
    fn health(&self) -> HealthStatus;

    /// Shut down the adapter gracefully.
    async fn shutdown(&mut self) -> Result<(), AdapterError>;

    /// Return recent stderr lines from the adapter (if any).
    ///
    /// The default implementation returns an empty list. Adapters that capture
    /// stderr (e.g. STDIO) override this to return buffered output.
    async fn stderr_lines(&self) -> Vec<String> {
        vec![]
    }

    /// Return recent activity log lines (e.g. tool call records).
    ///
    /// The default implementation returns an empty list. Adapters that record
    /// tool call activity (e.g. SSE, HTTP) override this.
    async fn activity_log(&self) -> Vec<String> {
        vec![]
    }

    /// Return the sanitized server name reported by the MCP server during initialize.
    ///
    /// The default implementation returns `None`. Adapters that capture
    /// `serverInfo.name` from the initialize response override this.
    #[allow(dead_code)] // Will be used by upstream callers once prefix routing is wired
    fn server_type(&self) -> Option<String> {
        None
    }

    /// Upstream-derived server name (sanitized + suffix-stripped) independent
    /// of any `server_type_override`. Returns `None` until the initialize
    /// handshake has populated it; adapters that do not capture
    /// `serverInfo.name` keep the default.
    ///
    /// This mirrors what [`McpAdapter::server_type`] would have returned if no
    /// override were configured, and lets the management API surface the
    /// "default name the upstream reports" alongside the effective name so the
    /// desktop UI can show users what they would revert to.
    fn upstream_server_name(&self) -> Option<String> {
        None
    }

    /// Returns the configured `server_type_override` (sanitized through the
    /// existing `effective_server_type` resolver with `None` for the upstream
    /// value), or `None` if no override is configured. Used by the registry to
    /// advertise endpoints before their first successful `initialize` handshake.
    fn configured_server_type(&self) -> Option<String> {
        None
    }

    /// Subscribe to MCP `notifications/tools/list_changed` ticks emitted by the
    /// underlying server. Each `recv()` represents at least one change
    /// notification observed since the previous receive; the registry treats
    /// every tick as an opaque cache-invalidation signal.
    ///
    /// The default implementation returns `None`, indicating the adapter does
    /// not surface tools-changed events. Adapters that handle the MCP
    /// notification override this and return a fresh `Receiver` from a
    /// long-lived `broadcast::Sender`.
    fn subscribe_tools_changed(&self) -> Option<tokio::sync::broadcast::Receiver<()>> {
        None
    }

    /// Latest container resource stats for the endpoint.
    ///
    /// The default implementation returns `None` (direct-spawn endpoints and
    /// transports that never run containers). The STDIO adapter overrides
    /// this with the most recent sample from its background stats poller
    /// when the endpoint runs inside a container.
    fn container_stats(&self) -> Option<crate::container_stats::ContainerStats> {
        None
    }

    /// Isolation outcome (configured vs actual) of the endpoint's last spawn.
    ///
    /// The default implementation returns `None` (transports that never spawn
    /// processes). The STDIO adapter overrides this with the outcome recorded
    /// at spawn time, including the configured-container → direct-spawn
    /// fallback when no container runtime is available.
    fn isolation_state(&self) -> Option<crate::adapter::stdio::IsolationState> {
        None
    }

    /// Wire a shared [`crate::events::ToolCallEventBus`] into the adapter so
    /// `call_tool` can publish typed `started` / `completed` / `failed`
    /// events for the desktop overlay's SSE stream.
    ///
    /// Default impl is a no-op so placeholder adapters
    /// ([`FailedAdapter`], [`StartingAdapter`]) and any future read-only
    /// adapters don't need to participate. Concrete transports (STDIO, SSE,
    /// HTTP, OAuth) override this and stash the bus for use during
    /// `call_tool`.
    fn set_event_bus(&self, _bus: crate::events::ToolCallEventBus) {}
}

/// A placeholder adapter registered when the real adapter fails to initialize.
///
/// Reports [`HealthStatus::Unhealthy`] so the endpoint appears in the management
/// UI as offline. Restarting the endpoint via the management API will call
/// `initialize()` on the real adapter again (handled by the restart endpoint).
pub struct FailedAdapter {
    error_message: String,
    /// Sanitized `server_type_override` carried over from the originating
    /// `EndpointConfig`. Surfaced via [`McpAdapter::configured_server_type`]
    /// so endpoints with a configured override still appear in the
    /// advertisement list when their real adapter fails to initialize.
    server_type_override: Option<String>,
}

impl FailedAdapter {
    /// Create a new failed adapter with the error message from initialization.
    pub fn new(error_message: String) -> Self {
        Self {
            error_message,
            server_type_override: None,
        }
    }

    /// Attach the originating endpoint's `server_type_override` so this
    /// failed adapter still advertises its configured type via
    /// [`McpAdapter::configured_server_type`].
    pub fn with_server_type_override(mut self, override_field: Option<String>) -> Self {
        self.server_type_override = override_field;
        self
    }
}

#[async_trait]
impl McpAdapter for FailedAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        // A failed adapter cannot be re-initialized in place.
        // The restart endpoint should replace this with a real adapter.
        Err(AdapterError::ConnectionFailed(format!(
            "server failed to initialize: {}",
            self.error_message
        )))
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![])
    }

    async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, AdapterError> {
        Err(AdapterError::ConnectionFailed(format!(
            "server failed to initialize: {}",
            self.error_message
        )))
    }

    fn health(&self) -> HealthStatus {
        HealthStatus::Unhealthy(self.error_message.clone())
    }

    fn configured_server_type(&self) -> Option<String> {
        crate::adapter::server_type_resolution::effective_server_type(
            self.server_type_override.clone(),
            None,
        )
        .map(|s| s.to_lowercase())
    }

    async fn stderr_lines(&self) -> Vec<String> {
        vec![format!("[ERROR] {}", self.error_message)]
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// A placeholder adapter registered while the real adapter is initializing.
///
/// Reports [`HealthStatus::Starting`] so the endpoint appears in the management
/// UI with a spinner. Once initialization completes, the caller replaces this
/// adapter in the registry with the real (or failed) adapter.
pub struct StartingAdapter;

#[async_trait]
impl McpAdapter for StartingAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        Err(AdapterError::NotInitialized)
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![])
    }

    async fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, AdapterError> {
        Err(AdapterError::NotInitialized)
    }

    fn health(&self) -> HealthStatus {
        HealthStatus::Starting
    }

    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A validation-failed endpoint registered as a `FailedAdapter` should
    /// still surface its configured `server_type_override` via
    /// [`McpAdapter::configured_server_type`], mirroring the bootstrap path in
    /// `main.rs` and the runtime path in `watcher.rs`.
    #[test]
    fn failed_adapter_surfaces_server_type_override() {
        let adapter = FailedAdapter::new("validation failed".to_string())
            .with_server_type_override(Some("Broken".to_string()));
        assert_eq!(adapter.configured_server_type(), Some("broken".to_string()));
    }

    /// Without an override, `configured_server_type` returns `None` so the
    /// endpoint is omitted from the advertisement list until it transitions
    /// to a real adapter that knows its server type.
    #[test]
    fn failed_adapter_without_override_returns_none() {
        let adapter = FailedAdapter::new("validation failed".to_string());
        assert_eq!(adapter.configured_server_type(), None);
    }

    #[test]
    fn truncate_for_display_passes_short_input_unchanged() {
        assert_eq!(truncate_for_display("short body", 200), "short body");
        let exact: String = "a".repeat(200);
        assert_eq!(truncate_for_display(&exact, 200), exact);
    }

    #[test]
    fn truncate_for_display_truncates_long_input_with_ellipsis() {
        let body: String = "a".repeat(500);
        let out = truncate_for_display(&body, 200);
        assert!(
            out.ends_with('…'),
            "expected trailing ellipsis, got {out:?}"
        );
        let n_chars = out.chars().count();
        assert_eq!(n_chars, 201, "expected 200 chars + ellipsis, got {n_chars}");
    }

    #[test]
    fn truncate_for_display_respects_char_boundaries_for_multibyte() {
        // A 300-char string of multi-byte chars must truncate on a char
        // boundary (no UTF-8 panic) and stay within max_chars + 1.
        let body: String = "✓".repeat(300);
        let out = truncate_for_display(&body, 200);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 201);
    }

    #[test]
    fn http_error_display_truncates_oversized_body() {
        let body: String = "x".repeat(1024);
        let err = AdapterError::HttpError {
            status: 500,
            body: body.clone(),
        };
        let rendered = err.to_string();
        assert!(rendered.starts_with("HTTP error 500: "));
        assert!(rendered.ends_with('…'));
        // "HTTP error 500: " (16 chars) + 200 chars + "…" (1 char) = 217.
        assert_eq!(rendered.chars().count(), 16 + HTTP_ERROR_BODY_MAX_CHARS + 1);
    }

    #[test]
    fn http_error_display_preserves_short_body_verbatim() {
        let err = AdapterError::HttpError {
            status: 404,
            body: "not found".to_string(),
        };
        assert_eq!(err.to_string(), "HTTP error 404: not found");
    }
}
