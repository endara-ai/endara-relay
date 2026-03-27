use crate::js_sandbox::MetaToolHandler;
use crate::registry::AdapterRegistry;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive},
        Sse,
    },
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;
use tracing::info;

/// Application state shared across all routes.
#[derive(Clone)]
pub struct AppState {
    pub registry: AdapterRegistry,
    pub js_execution_mode: Arc<AtomicBool>,
    pub meta_tool_handler: Arc<MetaToolHandler>,
}

/// JSON-RPC request body expected by MCP routes.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct JsonRpcBody {
    jsonrpc: Option<String>,
    method: Option<String>,
    params: Option<Value>,
    id: Option<Value>,
}

fn jsonrpc_response(id: Option<Value>, result: Value) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    }))
}

fn jsonrpc_error(id: Option<Value>, code: i64, message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": message },
            "id": id,
        })),
    )
}

/// POST /mcp/initialize
async fn mcp_initialize(Json(body): Json<JsonRpcBody>) -> Json<Value> {
    jsonrpc_response(
        body.id,
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "endara-relay",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

/// Build the 3 meta-tool definitions as JSON values.
fn meta_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_tools",
            "description": "List available tools with pagination",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "search_tools",
            "description": "Search tools by keyword",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "execute_tools",
            "description": "Execute JavaScript that calls tools",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "script": { "type": "string" }
                },
                "required": ["script"]
            }
        }),
    ]
}

/// POST /mcp/tools/list
async fn mcp_tools_list(
    State(state): State<AppState>,
    Json(body): Json<JsonRpcBody>,
) -> Json<Value> {
    let js_mode = state.js_execution_mode.load(Ordering::Relaxed);
    let meta_tools = meta_tool_definitions();

    let tools: Vec<Value> = if js_mode {
        // JS execution mode: only the 3 meta-tools
        meta_tools
    } else {
        // Normal mode: full prefixed catalog + 3 meta-tools
        let catalog = state.registry.merged_catalog().await;
        let mut tools: Vec<Value> = catalog
            .into_iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect();
        tools.extend(meta_tools);
        tools
    };

    jsonrpc_response(body.id, json!({ "tools": tools }))
}

/// POST /mcp/tools/call
async fn mcp_tools_call(
    State(state): State<AppState>,
    Json(body): Json<JsonRpcBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let params = body.params.unwrap_or(json!({}));
    let tool_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| jsonrpc_error(body.id.clone(), -32602, "missing 'name' in params"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));

    let js_mode = state.js_execution_mode.load(Ordering::Relaxed);

    // Check if this is a meta-tool call
    match tool_name {
        "list_tools" => {
            let limit = arguments.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
            let offset = arguments.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
            match state.meta_tool_handler.list_tools(limit, offset).await {
                Ok(result) => return Ok(jsonrpc_response(body.id, result)),
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        "search_tools" => {
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = arguments.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
            match state.meta_tool_handler.search_tools(query, limit).await {
                Ok(result) => return Ok(jsonrpc_response(body.id, result)),
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        "execute_tools" => {
            let script = arguments
                .get("script")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match state.meta_tool_handler.execute_tools(script).await {
                Ok(result) => return Ok(jsonrpc_response(body.id, result)),
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        _ => {}
    }

    // Not a meta-tool — if JS mode is on, reject direct tool calls
    if js_mode {
        return Err(jsonrpc_error(
            body.id,
            -32601,
            "Direct tool calls are not allowed in JS execution mode. Use execute_tools instead.",
        ));
    }

    match state
        .registry
        .route_tool_call(tool_name, arguments)
        .await
    {
        Ok(result) => Ok(jsonrpc_response(body.id, result)),
        Err(e) => Err(jsonrpc_error(body.id, -32603, &e.to_string())),
    }
}

/// GET /mcp/sse — basic SSE transport (sends a connection ack, then keeps alive)
async fn mcp_sse() -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);

    tokio::spawn(async move {
        let _ = tx
            .send(Ok(Event::default()
                .event("endpoint")
                .data("/mcp")))
            .await;
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Build the axum Router with all MCP routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/mcp/initialize", post(mcp_initialize))
        .route("/mcp/tools/list", post(mcp_tools_list))
        .route("/mcp/tools/call", post(mcp_tools_call))
        .route("/mcp/sse", get(mcp_sse))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the HTTP server and return the bound address.
/// The server runs until the provided shutdown signal completes.
pub async fn start_server(
    router: Router,
    addr: SocketAddr,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    info!(addr = %local_addr, "MCP HTTP server listening");

    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                tokio::signal::ctrl_c().await.ok();
                info!("Shutdown signal received");
            })
            .await
            .ok();
    });

    Ok((local_addr, handle))
}

