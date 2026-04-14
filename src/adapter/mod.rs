pub mod http;
pub mod oauth;
pub mod server_name;
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

    #[error("HTTP error {status}: {body}")]
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
}

/// A placeholder adapter registered when the real adapter fails to initialize.
///
/// Reports [`HealthStatus::Unhealthy`] so the endpoint appears in the management
/// UI as offline. Restarting the endpoint via the management API will call
/// `initialize()` on the real adapter again (handled by the restart endpoint).
pub struct FailedAdapter {
    error_message: String,
}

impl FailedAdapter {
    /// Create a new failed adapter with the error message from initialization.
    pub fn new(error_message: String) -> Self {
        Self { error_message }
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
