use crate::js_sandbox::MetaToolHandler;
use crate::oauth::OAuthFlowManager;
use crate::registry::AdapterRegistry;
use crate::token_manager::TokenManager;
use crate::OAuthTokenNotifiers;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive},
        Html, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;

use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

/// Application state shared across all routes.
#[derive(Clone)]
pub struct AppState {
    pub registry: AdapterRegistry,
    pub js_execution_mode: Arc<AtomicBool>,
    pub meta_tool_handler: Arc<MetaToolHandler>,
    /// OAuth flow manager (shared with management routes).
    pub oauth_flow_manager: Option<Arc<OAuthFlowManager>>,
    /// Token manager for persisting OAuth tokens.
    pub token_manager: Option<Arc<TokenManager>>,
    /// Per-endpoint token notifiers: endpoint_name -> watch::Sender.
    pub oauth_token_notifiers: Option<OAuthTokenNotifiers>,
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
                let mut tool = json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                });
                if let Some(annotations) = t.annotations {
                    tool["annotations"] = annotations;
                }
                tool
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
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let js_mode = state.js_execution_mode.load(Ordering::Relaxed);

    // Check if this is a meta-tool call
    match tool_name {
        "list_tools" => {
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let offset = arguments
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
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
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
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

    match state.registry.route_tool_call(tool_name, arguments).await {
        Ok(result) => Ok(jsonrpc_response(body.id, result)),
        Err(e) => Err(jsonrpc_error(body.id, -32603, &e.to_string())),
    }
}

/// GET /mcp/sse — basic SSE transport (sends a connection ack, then keeps alive)
async fn mcp_sse() -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);

    tokio::spawn(async move {
        let _ = tx
            .send(Ok(Event::default().event("endpoint").data("/mcp")))
            .await;
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Query params for the OAuth callback.
#[derive(Deserialize)]
struct OAuthCallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// GET /oauth/callback
///
/// The OAuth authorization server redirects the user's browser here after login.
/// Exchanges the authorization code for tokens, saves them, and signals the adapter.
async fn oauth_callback(
    State(state): State<AppState>,
    Query(params): Query<OAuthCallbackParams>,
) -> Html<String> {
    // Handle OAuth error
    if let Some(ref err) = params.error {
        warn!(error = %err, "OAuth callback received error");
        return Html(format!(
            "<html><body><h1>OAuth Error</h1><p>{}</p><p>You can close this window.</p></body></html>",
            err
        ));
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            return Html(
                "<html><body><h1>OAuth Error</h1><p>Missing authorization code.</p><p>You can close this window.</p></body></html>"
                    .to_string(),
            );
        }
    };
    let state_param = match params.state {
        Some(s) => s,
        None => {
            return Html(
                "<html><body><h1>OAuth Error</h1><p>Missing state parameter.</p><p>You can close this window.</p></body></html>"
                    .to_string(),
            );
        }
    };

    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return Html(
            "<html><body><h1>OAuth Error</h1><p>OAuth not configured.</p></body></html>"
                .to_string(),
        );
    };

    let flow = match flow_mgr.consume_flow(&state_param).await {
        Some(f) => f,
        None => {
            warn!(state = %state_param, "Invalid or expired OAuth state");
            return Html(
                "<html><body><h1>OAuth Error</h1><p>Invalid or expired login session. Please try again.</p><p>You can close this window.</p></body></html>"
                    .to_string(),
            );
        }
    };

    // Exchange authorization code for tokens
    let client = reqwest::Client::new();
    let mut form_parts: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code),
        ("redirect_uri".into(), flow.redirect_uri.clone()),
        ("client_id".into(), flow.client_id.clone()),
        ("code_verifier".into(), flow.code_verifier.clone()),
    ];
    if let Some(ref secret) = flow.client_secret {
        form_parts.push(("client_secret".into(), secret.clone()));
    }

    let form_body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form_parts.iter())
        .finish();

    let token_response: reqwest::Response = match client
        .post(&flow.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!(error = %e, "Failed to exchange authorization code");
            return Html(format!(
                "<html><body><h1>OAuth Error</h1><p>Failed to exchange code: {}</p><p>You can close this window.</p></body></html>",
                e
            ));
        }
    };

    if !token_response.status().is_success() {
        let status = token_response.status();
        let body = token_response.text().await.unwrap_or_default();
        error!(%status, body = %body, "Token endpoint returned error");
        return Html(format!(
            "<html><body><h1>OAuth Error</h1><p>Token endpoint returned {}: {}</p><p>You can close this window.</p></body></html>",
            status, body
        ));
    }

    let token_json: serde_json::Value = match token_response.json().await {
        Ok(v) => v,
        Err(e) => {
            error!(error = %e, "Failed to parse token response");
            return Html(format!(
                "<html><body><h1>OAuth Error</h1><p>Invalid token response: {}</p><p>You can close this window.</p></body></html>",
                e
            ));
        }
    };

    let access_token = token_json["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return Html(
            "<html><body><h1>OAuth Error</h1><p>No access_token in response.</p><p>You can close this window.</p></body></html>"
                .to_string(),
        );
    }

    let token_set = crate::token_manager::TokenSet {
        access_token: access_token.clone(),
        refresh_token: token_json["refresh_token"].as_str().map(|s| s.to_string()),
        expires_at: token_json["expires_in"].as_u64().map(|secs| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + secs
        }),
        token_type: token_json["token_type"]
            .as_str()
            .unwrap_or("Bearer")
            .to_string(),
        scope: token_json["scope"].as_str().map(|s| s.to_string()),
    };

    // Save tokens to disk
    if let Some(ref tm) = state.token_manager {
        if let Err(e) = tm.save(&flow.endpoint_name, &token_set).await {
            error!(endpoint = %flow.endpoint_name, error = %e, "Failed to save tokens");
        }
    }

    // Signal the adapter via watch channel
    if let Some(ref notifiers) = state.oauth_token_notifiers {
        let notifiers = notifiers.read().await;
        if let Some(tx) = notifiers.get(&flow.endpoint_name) {
            let _ = tx.send(Some(access_token));
            info!(endpoint = %flow.endpoint_name, "Token notification sent to adapter");
        }
    }

    Html(format!(
        "<html><body><h1>Login Successful</h1><p>Endpoint <strong>{}</strong> is now authenticated.</p><p>You can close this window.</p></body></html>",
        flow.endpoint_name
    ))
}

/// Build the axum Router with all MCP routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/mcp/initialize", post(mcp_initialize))
        .route("/mcp/tools/list", post(mcp_tools_list))
        .route("/mcp/tools/call", post(mcp_tools_call))
        .route("/mcp/sse", get(mcp_sse))
        .route("/oauth/callback", get(oauth_callback))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Create a future that resolves when a shutdown signal (SIGINT, SIGTERM, or SIGHUP) is received.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(unix)]
    let hangup = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("failed to install SIGHUP handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    #[cfg(not(unix))]
    let hangup = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("SIGINT received, shutting down");
        }
        _ = terminate => {
            info!("SIGTERM received, shutting down");
        }
        _ = hangup => {
            info!("SIGHUP received, shutting down");
        }
    }
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
            .with_graceful_shutdown(shutdown_signal())
            .await
            .ok();
    });

    Ok((local_addr, handle))
}
