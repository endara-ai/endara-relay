//! Mock MCP server fixture that requires Bearer token authentication.
//!
//! Simulates an upstream MCP server that the OAuth adapter talks to.
//! Returns 401 Unauthorized if no valid Bearer token is provided.
//! Handles initialize, tools/list, and tools/call JSON-RPC methods.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

/// A mock MCP server that requires Bearer token authentication.
#[allow(dead_code)]
pub struct MockMcpServer {
    port: u16,
    state: MockMcpState,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone)]
struct MockMcpState {
    inner: Arc<MockMcpStateInner>,
}

struct MockMcpStateInner {
    /// When true, returns 401 for any request (simulates revoked token).
    force_401: RwLock<bool>,
    /// Count of requests received.
    request_count: RwLock<usize>,
    /// When true, omits `name` from `serverInfo` in initialize response.
    omit_server_info_name: bool,
}

#[allow(dead_code)]
impl MockMcpServer {
    /// Start a mock MCP server on a random free port.
    pub async fn start() -> Self {
        Self::start_inner(false).await
    }

    /// Start a mock MCP server that omits `name` from `serverInfo`.
    pub async fn start_without_server_name() -> Self {
        Self::start_inner(true).await
    }

    async fn start_inner(omit_server_info_name: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind mock MCP server");
        let port = listener.local_addr().unwrap().port();

        let state = MockMcpState {
            inner: Arc::new(MockMcpStateInner {
                force_401: RwLock::new(false),
                request_count: RwLock::new(0),
                omit_server_info_name,
            }),
        };

        let app = Router::new()
            .route("/mcp", post(handle_mcp))
            .with_state(state.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("mock MCP server failed");
        });

        Self {
            port,
            state,
            shutdown_tx,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url())
    }

    /// Force all requests to return 401 (simulates revoked token at provider).
    pub async fn set_force_401(&self, force: bool) {
        *self.state.inner.force_401.write().await = force;
    }

    /// Get the total number of requests received.
    pub async fn request_count(&self) -> usize {
        *self.state.inner.request_count.read().await
    }
}

/// Handle MCP JSON-RPC requests with Bearer token auth check.
async fn handle_mcp(
    State(state): State<MockMcpState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Increment request count
    {
        let mut count = state.inner.request_count.write().await;
        *count += 1;
    }

    // Check for forced 401
    if *state.inner.force_401.read().await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    // Check Bearer token
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !auth.starts_with("Bearer ") || auth.len() <= 7 {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    let method = body["method"].as_str().unwrap_or("");
    let id = body["id"].as_u64().unwrap_or(0);
    let params = body.get("params").cloned().unwrap_or(json!({}));

    let response = match method {
        "initialize" => {
            let server_info = if state.inner.omit_server_info_name {
                // Omit `name` entirely from serverInfo
                json!({"version": "0.1.0"})
            } else {
                json!({"name": "mock-oauth-mcp", "version": "0.1.0"})
            };
            json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": server_info
                },
                "id": id
            })
        }
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echoes input back",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"message": {"type": "string"}},
                        "required": ["message"]
                    }
                }]
            },
            "id": id
        }),
        "tools/call" => {
            let msg = params
                .get("arguments")
                .and_then(|a| a.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            json!({
                "jsonrpc": "2.0",
                "result": {
                    "content": [{"type": "text", "text": format!("echo: {}", msg)}],
                    "isError": false
                },
                "id": id
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "error": {"code": -32601, "message": "Method not found"},
            "id": id
        }),
    };

    Json(response).into_response()
}
