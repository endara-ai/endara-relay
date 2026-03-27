pub mod http;
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

    #[error("adapter is shutting down")]
    ShuttingDown,

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("HTTP error: {0}")]
    HttpError(String),

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
}

