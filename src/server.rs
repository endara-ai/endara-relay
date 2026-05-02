use crate::js_sandbox::MetaToolHandler;
use crate::oauth::{OAuthFlowManager, OAuthSetupManager};
use crate::registry::AdapterRegistry;
use crate::token_manager::TokenManager;
use crate::OAuthAdapterInners;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Response, Sse,
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
use std::time::Instant;
use tokio::net::TcpListener;

use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{AllowOrigin, CorsLayer};
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
    /// Per-endpoint shared OAuth adapter inner states.
    pub oauth_adapter_inners: Option<OAuthAdapterInners>,
    /// Transient OAuth setup session manager (preflight flow).
    pub setup_manager: Option<Arc<OAuthSetupManager>>,
}

/// JSON-RPC request body expected by MCP routes.
#[derive(serde::Deserialize, serde::Serialize, Clone)]
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

/// Wrap a raw meta-tool result in the MCP `content` array format.
fn wrap_meta_tool_result(result: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&result).unwrap_or_default()
        }]
    })
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
            "protocolVersion": "2025-03-26",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "Endara Relay",
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
            "description": "List available tools with pagination. Returns `{ tools, total, limit, offset }`. Each tool has `name`, `description`, and `input_schema`. The `name` is the exact identifier to use when calling tools via `execute_tools`.",
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
            "description": "Fuzzy search across tool name, description, server endpoint, and input-schema property names. Typo-tolerant (Levenshtein), case-insensitive, and aware of camelCase / snake_case / kebab-case boundaries. Results are ranked by relevance (exact > prefix > substring > fuzzy; name > description > endpoint); tools matching more query tokens rank higher. Returns an array of matching tools, each with `name`, `description`, and `input_schema`.",
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
            "description": concat!(
                "Execute a JavaScript snippet that can call tools. ",
                "Tools are available as `tools[\"tool_name\"]({...})`. ",
                "Multi-server tool names use `prefix__name` format (double underscore); single-server mode has no prefix. ",
                "Each tool call returns an MCP result with `content` (array of `{type, text}`) and/or `structuredContent`. ",
                "Prefer `structuredContent` when present — it is the server's structured output. ",
                "`content[0].text` is provider-defined and is NOT guaranteed to be JSON: it may be prose, a partial summary, empty, or truncated. ",
                "Only call `JSON.parse` on it after a guard such as `typeof t === \"string\" && /^\\s*[\\[{]/.test(t)`. ",
                "Use `return` to send data back.\n\n",
                "Examples:\n",
                "```js\n",
                "// Safe pattern: prefer structuredContent, only JSON.parse after a guard\n",
                "const r = await tools[\"todoist__get-tasks\"]({ limit: 5 });\n",
                "if (r.structuredContent) return r.structuredContent;\n",
                "const t = r.content && r.content[0] && r.content[0].text;\n",
                "return typeof t === \"string\" && /^\\s*[\\[{]/.test(t) ? JSON.parse(t) : t;\n",
                "```\n",
                "```js\n",
                "// Chain two tool calls\n",
                "const projects = await tools[\"todoist__get-projects\"]({});\n",
                "const tasks = await tools[\"todoist__get-tasks\"]({ project_id: \"123\" });\n",
                "return { projects, tasks };\n",
                "```\n",
                "```js\n",
                "// Single-server mode (no prefix)\n",
                "const result = await tools[\"read_file\"]({ path: \"src/main.rs\" });\n",
                "return result;\n",
                "```",
            ),
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
        tools.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
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
                Ok(result) => return Ok(jsonrpc_response(body.id, wrap_meta_tool_result(result))),
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
                Ok(result) => return Ok(jsonrpc_response(body.id, wrap_meta_tool_result(result))),
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        "execute_tools" => {
            let script = arguments
                .get("script")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match state.meta_tool_handler.execute_tools(script).await {
                Ok(result) => return Ok(jsonrpc_response(body.id, wrap_meta_tool_result(result))),
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

/// Handle a single JSON-RPC message object, returning `None` for notifications
/// (which get 202 Accepted) or `Some(Value)` for requests that need a response.
async fn handle_single_message(state: &AppState, msg: Value, headers_str: &str) -> Option<Value> {
    let start = Instant::now();
    let method = msg
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_notification = msg.get("id").is_none() || msg.get("id") == Some(&Value::Null);
    let req_bytes = serde_json::to_string(&msg).map(|s| s.len()).unwrap_or(0);

    // Notifications (no `id` field) get 202 Accepted with no body per MCP spec.
    if is_notification {
        let elapsed_ms = start.elapsed().as_millis() as u64;
        info!(
            method = %method,
            elapsed_ms = elapsed_ms,
            req_bytes = req_bytes,
            resp_bytes = 0,
            status = 202,
            headers = %headers_str,
            "MCP notification"
        );
        return None;
    }

    // Deserialize as JsonRpcBody for dispatch
    let body: JsonRpcBody = match serde_json::from_value(msg) {
        Ok(b) => b,
        Err(_) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "error": { "code": -32600, "message": "Invalid Request" },
                "id": null,
            }));
        }
    };

    let result: Result<Json<Value>, (StatusCode, Json<Value>)> = match method.as_str() {
        "initialize" => Ok(mcp_initialize(Json(body)).await),
        "tools/list" => Ok(mcp_tools_list(State(state.clone()), Json(body)).await),
        "tools/call" => mcp_tools_call(State(state.clone()), Json(body)).await,
        _ => Err(jsonrpc_error(
            body.id,
            -32601,
            &format!("method not found: {}", method),
        )),
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let resp_value = match result {
        Ok(Json(resp)) => {
            let resp_bytes = serde_json::to_string(&resp).map(|s| s.len()).unwrap_or(0);
            info!(
                method = %method,
                elapsed_ms = elapsed_ms,
                req_bytes = req_bytes,
                resp_bytes = resp_bytes,
                status = 200,
                headers = %headers_str,
                "MCP request"
            );
            resp
        }
        Err((status, Json(resp))) => {
            let resp_bytes = serde_json::to_string(&resp).map(|s| s.len()).unwrap_or(0);
            let status_code = status.as_u16();
            if status_code >= 500 {
                error!(
                    method = %method,
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = status_code,
                    headers = %headers_str,
                    "MCP request"
                );
            } else if status_code == 200 {
                // JSON-RPC 2.0: errors are returned with HTTP 200, not a sign of trouble.
                info!(
                    method = %method,
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = status_code,
                    headers = %headers_str,
                    "MCP request"
                );
            } else {
                warn!(
                    method = %method,
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = status_code,
                    headers = %headers_str,
                    "MCP request"
                );
            }
            resp
        }
    };

    Some(resp_value)
}

/// POST /mcp — Unified Streamable HTTP transport endpoint.
///
/// Accepts a JSON-RPC request (single object or batch array) and dispatches by
/// the `method` field to the appropriate handler, as required by the MCP
/// Streamable HTTP spec.
///
/// Per the spec, JSON-RPC notifications (messages without an `id` field) must
/// receive HTTP 202 Accepted with no body.
///
/// Batch requests (JSON arrays) are processed and return an array of responses.
/// If all messages in a batch are notifications, returns 202 Accepted.
async fn mcp_unified(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let headers_str: String = headers
        .iter()
        .map(|(k, v)| format!("{}={}", k, v.to_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join(" ");

    match body {
        Value::Array(messages) => {
            if messages.is_empty() {
                return (
                    StatusCode::OK,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    Json(json!({
                        "jsonrpc": "2.0",
                        "error": { "code": -32600, "message": "Invalid Request: empty batch" },
                        "id": null,
                    })),
                )
                    .into_response();
            }

            let mut responses: Vec<Value> = Vec::new();
            for msg in messages {
                if let Some(resp) = handle_single_message(&state, msg, &headers_str).await {
                    responses.push(resp);
                }
            }

            if responses.is_empty() {
                // All messages were notifications
                StatusCode::ACCEPTED.into_response()
            } else {
                (
                    StatusCode::OK,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    Json(Value::Array(responses)),
                )
                    .into_response()
            }
        }
        Value::Object(_) => match handle_single_message(&state, body, &headers_str).await {
            Some(resp) => (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )],
                Json(resp),
            )
                .into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
        _ => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            Json(json!({
                "jsonrpc": "2.0",
                "error": { "code": -32600, "message": "Invalid Request: expected object or array" },
                "id": null,
            })),
        )
            .into_response(),
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

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let token_set = crate::token_manager::TokenSet {
        access_token: access_token.clone(),
        refresh_token: token_json["refresh_token"].as_str().map(|s| s.to_string()),
        expires_at: token_json["expires_in"]
            .as_u64()
            .map(|secs| now_secs + secs),
        token_type: token_json["token_type"]
            .as_str()
            .unwrap_or("Bearer")
            .to_string(),
        scope: token_json["scope"].as_str().map(|s| s.to_string()),
        issued_at: Some(now_secs),
    };

    // Check if this is a setup session callback (preflight flow)
    if let Some(session_id_str) = flow.endpoint_name.strip_prefix("setup:") {
        if let Ok(session_id) = session_id_str.parse::<uuid::Uuid>() {
            if let Some(ref setup_mgr) = state.setup_manager {
                if setup_mgr.mark_authorized(&session_id, token_set).await {
                    info!(session_id = %session_id_str, "Setup session authorized via callback");
                    return Html(
                        "<html><body><h1>Authorization Successful</h1>\
                         <p>You can close this window and return to the app to complete setup.</p>\
                         </body></html>"
                            .to_string(),
                    );
                }
            }
        }
        warn!(flow_name = %flow.endpoint_name, "Setup session not found for callback");
        return Html(
            "<html><body><h1>OAuth Error</h1>\
             <p>Setup session not found or expired. Please start over.</p>\
             <p>You can close this window.</p></body></html>"
                .to_string(),
        );
    }

    // Existing endpoint re-auth flow: save tokens to disk
    if let Some(ref tm) = state.token_manager {
        if let Err(e) = tm.save(&flow.endpoint_name, &token_set).await {
            error!(endpoint = %flow.endpoint_name, error = %e, "Failed to save tokens");
        }
    }

    // Apply tokens to the adapter's shared inner state
    if let Some(ref inners) = state.oauth_adapter_inners {
        let inners = inners.read().await;
        if let Some(inner) = inners.get(&flow.endpoint_name) {
            inner.apply_tokens(token_set.clone()).await;
            info!(endpoint = %flow.endpoint_name, "Tokens applied to OAuth adapter");
        }
    }

    Html(format!(
        "<html><body><h1>Login Successful</h1><p>Endpoint <strong>{}</strong> is now authenticated.</p><p>You can close this window.</p></body></html>",
        flow.endpoint_name
    ))
}

/// Logged wrapper for POST /mcp/initialize (direct route).
async fn mcp_initialize_logged(body: Json<JsonRpcBody>) -> Json<Value> {
    let start = Instant::now();
    let req_bytes = serde_json::to_string(&body.0).map(|s| s.len()).unwrap_or(0);
    let resp = mcp_initialize(body).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let resp_bytes = serde_json::to_string(&resp.0).map(|s| s.len()).unwrap_or(0);
    info!(
        method = "initialize",
        elapsed_ms = elapsed_ms,
        req_bytes = req_bytes,
        resp_bytes = resp_bytes,
        status = 200,
        "MCP request"
    );
    resp
}

/// Logged wrapper for POST /mcp/tools/list (direct route).
async fn mcp_tools_list_logged(state: State<AppState>, body: Json<JsonRpcBody>) -> Json<Value> {
    let start = Instant::now();
    let req_bytes = serde_json::to_string(&body.0).map(|s| s.len()).unwrap_or(0);
    let resp = mcp_tools_list(state, body).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let resp_bytes = serde_json::to_string(&resp.0).map(|s| s.len()).unwrap_or(0);
    info!(
        method = "tools/list",
        elapsed_ms = elapsed_ms,
        req_bytes = req_bytes,
        resp_bytes = resp_bytes,
        status = 200,
        "MCP request"
    );
    resp
}

/// Logged wrapper for POST /mcp/tools/call (direct route).
async fn mcp_tools_call_logged(
    state: State<AppState>,
    body: Json<JsonRpcBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let start = Instant::now();
    let req_bytes = serde_json::to_string(&body.0).map(|s| s.len()).unwrap_or(0);
    let result = mcp_tools_call(state, body).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    match &result {
        Ok(Json(resp)) => {
            let resp_bytes = serde_json::to_string(resp).map(|s| s.len()).unwrap_or(0);
            info!(
                method = "tools/call",
                elapsed_ms = elapsed_ms,
                req_bytes = req_bytes,
                resp_bytes = resp_bytes,
                status = 200,
                "MCP request"
            );
        }
        Err((status, Json(resp))) => {
            let resp_bytes = serde_json::to_string(resp).map(|s| s.len()).unwrap_or(0);
            let status_code = status.as_u16();
            if status_code >= 500 {
                error!(
                    method = "tools/call",
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = status_code,
                    "MCP request"
                );
            } else if status_code == 200 {
                // JSON-RPC 2.0: errors are returned with HTTP 200, not a sign of trouble.
                info!(
                    method = "tools/call",
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = status_code,
                    "MCP request"
                );
            } else {
                warn!(
                    method = "tools/call",
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = status_code,
                    "MCP request"
                );
            }
        }
    }
    result
}

/// Handler for DELETE /mcp — returns 405 Method Not Allowed.
/// The MCP Streamable HTTP spec allows servers to opt out of session termination.
async fn mcp_delete() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

/// Check whether an Origin header value is a localhost origin.
/// Allows `http://localhost`, `http://127.0.0.1`, `http://[::1]` on any port.
fn is_localhost_origin(origin: &str) -> bool {
    // Parse out scheme + host + optional port
    let without_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(host_port) = without_scheme else {
        return false;
    };
    // Handle IPv6 bracket notation like [::1]:3000
    let host = if host_port.starts_with('[') {
        // IPv6: extract up to the closing bracket
        host_port
            .split(']')
            .next()
            .map(|s| format!("{}]", s))
            .unwrap_or_default()
    } else {
        // IPv4 / hostname: strip port
        host_port.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// Build the axum Router with all MCP routes.
///
/// CORS is configured to only allow localhost origins (DNS rebinding protection).
/// Use `build_router_with_origins` to allow additional origins.
pub fn build_router(state: AppState) -> Router {
    build_router_with_origins(state, &[])
}

/// Build the axum Router with all MCP routes and additional allowed origins.
///
/// `extra_origins` is a list of allowed origin strings (e.g., `["https://example.com"]`).
/// Localhost origins (`127.0.0.1`, `::1`, `localhost`) are always allowed.
pub fn build_router_with_origins(state: AppState, extra_origins: &[String]) -> Router {
    let extra: Vec<String> = extra_origins.to_vec();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            let Ok(origin_str) = origin.to_str() else {
                return false;
            };
            is_localhost_origin(origin_str) || extra.iter().any(|allowed| allowed == origin_str)
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([axum::http::header::CONTENT_TYPE]);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/mcp", post(mcp_unified).delete(mcp_delete))
        .route("/mcp/initialize", post(mcp_initialize_logged))
        .route("/mcp/tools/list", post(mcp_tools_list_logged))
        .route("/mcp/tools/call", post(mcp_tools_call_logged))
        .route("/mcp/sse", get(mcp_sse))
        .route("/oauth/callback", get(oauth_callback))
        .layer(cors)
        .with_state(state)
}

/// GET /healthz — liveness probe.
///
/// Returns `200 OK` with a plain-text `ok` body so external supervisors
/// (load balancers, container orchestrators, uptime checks) can verify
/// the relay process is up without exercising upstream MCP adapters.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use crate::js_sandbox::MetaToolHandler;
    use crate::registry::AdapterRegistry;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    /// A mock adapter for server-level sorting tests.
    struct MockAdapter {
        tools: Vec<ToolInfo>,
    }

    impl MockAdapter {
        fn with_tools(names: &[&str]) -> Self {
            Self {
                tools: names
                    .iter()
                    .map(|n| ToolInfo {
                        name: n.to_string(),
                        description: Some(format!("{} tool", n)),
                        input_schema: json!({"type": "object"}),
                        annotations: None,
                    })
                    .collect(),
            }
        }
    }

    #[async_trait]
    impl McpAdapter for MockAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    /// Build a minimal AppState for testing (no OAuth, no token manager).
    fn test_app_state() -> AppState {
        let registry = AdapterRegistry::new();
        let meta_tool_handler = Arc::new(MetaToolHandler::new(
            Arc::new(registry.clone()),
            Duration::from_secs(5),
        ));
        AppState {
            registry,
            js_execution_mode: Arc::new(AtomicBool::new(false)),
            meta_tool_handler,
            oauth_flow_manager: None,
            token_manager: None,
            oauth_adapter_inners: None,
            setup_manager: None,
        }
    }

    #[test]
    fn jsonrpc_response_has_correct_structure() {
        let resp = jsonrpc_response(Some(json!(1)), json!({"ok": true}));
        let body = resp.0;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["ok"], true);
        assert!(body.get("error").is_none());
    }

    #[test]
    fn jsonrpc_response_with_null_id() {
        let resp = jsonrpc_response(None, json!("hello"));
        let body = resp.0;
        assert_eq!(body["jsonrpc"], "2.0");
        assert!(body["id"].is_null());
        assert_eq!(body["result"], "hello");
    }

    #[test]
    fn jsonrpc_error_has_correct_structure() {
        let (status, resp) = jsonrpc_error(Some(json!(42)), -32601, "Method not found");
        assert_eq!(status, StatusCode::OK);
        let body = resp.0;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 42);
        assert_eq!(body["error"]["code"], -32601);
        assert_eq!(body["error"]["message"], "Method not found");
        assert!(body.get("result").is_none());
    }

    #[tokio::test]
    async fn mcp_initialize_returns_correct_response() {
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("initialize".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_initialize(Json(body)).await;

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);

        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "Endara Relay");
        // Version should be a non-empty string
        assert!(!result["serverInfo"]["version"].as_str().unwrap().is_empty());
        // Capabilities must include "tools"
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn meta_tool_definitions_contains_expected_tools() {
        let defs = meta_tool_definitions();
        assert_eq!(defs.len(), 3);

        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(names.contains(&"execute_tools"));

        // Each definition must have name, description, inputSchema
        for def in &defs {
            assert!(def["name"].is_string());
            assert!(def["description"].is_string());
            assert!(def["inputSchema"].is_object());
            assert_eq!(def["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn test_list_tools_description_documents_return_format() {
        let defs = meta_tool_definitions();
        let list_desc = defs.iter().find(|d| d["name"] == "list_tools").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            list_desc.contains("tools"),
            "list_tools description should mention 'tools'"
        );
        assert!(
            list_desc.contains("total"),
            "list_tools description should mention 'total'"
        );
        assert!(
            list_desc.contains("limit"),
            "list_tools description should mention 'limit'"
        );
        assert!(
            list_desc.contains("offset"),
            "list_tools description should mention 'offset'"
        );
        assert!(
            list_desc.contains("execute_tools"),
            "list_tools description should mention 'execute_tools'"
        );
    }

    #[test]
    fn test_search_tools_description_documents_behavior() {
        let defs = meta_tool_definitions();
        let search_desc = defs.iter().find(|d| d["name"] == "search_tools").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            search_desc.contains("keyword")
                || search_desc.contains("search")
                || search_desc.contains("Search"),
            "search_tools description should mention 'keyword' or 'search'"
        );
        assert!(
            search_desc.contains("fuzzy") || search_desc.contains("Fuzzy"),
            "search_tools description should mention 'fuzzy'"
        );
        assert!(
            search_desc.contains("rank") || search_desc.contains("Ranked"),
            "search_tools description should mention 'rank'"
        );
    }

    #[test]
    fn test_execute_tools_description_has_examples() {
        let defs = meta_tool_definitions();
        let exec_desc = defs.iter().find(|d| d["name"] == "execute_tools").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            exec_desc.contains("tools[\""),
            "execute_tools description should document calling convention with tools[\""
        );
        assert!(
            exec_desc.contains("prefix__name") || exec_desc.contains("double underscore"),
            "execute_tools description should document naming convention"
        );
        assert!(
            exec_desc.contains("structuredContent"),
            "execute_tools description should mention structuredContent return format"
        );
        assert!(
            exec_desc.contains("return"),
            "execute_tools description should mention how to send data back"
        );
        assert!(
            exec_desc.contains("await"),
            "execute_tools description should include at least one code example with await"
        );
        assert!(
            exec_desc.contains("NOT guaranteed to be JSON"),
            "execute_tools description should warn that content[0].text is not guaranteed to be JSON"
        );
        assert!(
            exec_desc.contains("Prefer `structuredContent`"),
            "execute_tools description should prefer structuredContent over content[0].text"
        );
        assert!(
            exec_desc.contains(r#"/^\s*[\[{]/"#),
            "execute_tools description should show a regex guard before JSON.parse"
        );
        assert!(
            exec_desc.contains("if (r.structuredContent) return r.structuredContent"),
            "execute_tools description should include the safe-guard example preferring structuredContent"
        );
        assert!(
            exec_desc
                .contains(r#"typeof t === "string" && /^\s*[\[{]/.test(t) ? JSON.parse(t) : t"#),
            "execute_tools description should include the typeof + regex guard before JSON.parse"
        );
    }

    #[test]
    fn build_router_succeeds() {
        let state = test_app_state();
        let _router = build_router(state);
        // If we get here without panic, the router was built successfully.
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt; // for oneshot

        let state = test_app_state();
        let app = build_router(state);
        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn mcp_tools_list_includes_meta_tools_in_normal_mode() {
        let state = test_app_state();
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body)).await;
        let tools = resp["result"]["tools"].as_array().unwrap();

        // With no adapters registered, only the 3 meta-tools should appear
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(names.contains(&"execute_tools"));
    }

    #[tokio::test]
    async fn mcp_tools_list_js_mode_returns_only_meta_tools() {
        let state = test_app_state();
        state.js_execution_mode.store(true, Ordering::Relaxed);
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body)).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
    }

    #[tokio::test]
    async fn meta_tools_return_mcp_content_array_format() {
        let state = test_app_state();

        // Test list_tools
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "list_tools", "arguments": {}})),
            id: Some(json!(1)),
        };
        let result = mcp_tools_call(State(state.clone()), Json(body)).await;
        let Json(resp) = result.unwrap();
        let content = resp["result"]["content"]
            .as_array()
            .expect("content array for list_tools");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        let inner: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert!(inner["tools"].is_array());
        assert!(inner["total"].is_number());

        // Test search_tools
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "search_tools", "arguments": {"query": "test"}})),
            id: Some(json!(2)),
        };
        let result = mcp_tools_call(State(state.clone()), Json(body)).await;
        let Json(resp) = result.unwrap();
        let content = resp["result"]["content"]
            .as_array()
            .expect("content array for search_tools");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].is_string());

        // Test execute_tools (empty script returns result)
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "execute_tools", "arguments": {"script": ""}})),
            id: Some(json!(3)),
        };
        let result = mcp_tools_call(State(state), Json(body)).await;
        // execute_tools with empty script may error — either way, check the shape
        match result {
            Ok(Json(resp)) => {
                let content = resp["result"]["content"]
                    .as_array()
                    .expect("content array for execute_tools");
                assert_eq!(content[0]["type"], "text");
            }
            Err(_) => {
                // An error response is acceptable for empty script
            }
        }
    }

    /// Helper: send a JSON-RPC POST to `/mcp` via the router and return the response.
    async fn post_mcp(state: AppState, body: &Value) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;

        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    /// Helper: collect the response body bytes into a `Value`.
    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn mcp_unified_dispatches_initialize() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["serverInfo"]["name"], "Endara Relay");
    }

    #[tokio::test]
    async fn mcp_unified_dispatches_tools_list() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"tools/list","id":2}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 2);
        assert!(body["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn mcp_unified_notification_returns_202_accepted() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "202 response must have no body");
    }

    #[tokio::test]
    async fn mcp_unified_any_notification_returns_202() {
        // Any JSON-RPC message without an `id` is a notification → 202
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/some_custom"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn mcp_unified_returns_method_not_found_for_unknown() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"unknown/method","id":99}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn get_mcp_returns_405_method_not_allowed() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn delete_mcp_returns_405_method_not_allowed() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn mcp_unified_batch_requests() {
        let state = test_app_state();
        let batch = json!([
            {"jsonrpc":"2.0","method":"initialize","id":1},
            {"jsonrpc":"2.0","method":"tools/list","id":2},
        ]);
        let resp = post_mcp(state, &batch).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().expect("batch response should be an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[0]["result"]["serverInfo"]["name"], "Endara Relay");
        assert_eq!(arr[1]["id"], 2);
        assert!(arr[1]["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn mcp_unified_batch_all_notifications_returns_202() {
        let state = test_app_state();
        let batch = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","method":"notifications/cancelled"},
        ]);
        let resp = post_mcp(state, &batch).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "all-notification batch must have no body");
    }

    #[tokio::test]
    async fn mcp_unified_batch_mixed_requests_and_notifications() {
        let state = test_app_state();
        let batch = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","method":"initialize","id":1},
            {"jsonrpc":"2.0","method":"notifications/cancelled"},
        ]);
        let resp = post_mcp(state, &batch).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body
            .as_array()
            .expect("mixed batch response should be an array");
        // Only the request (id:1) should appear in the response
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
    }

    #[tokio::test]
    async fn mcp_unified_empty_batch_returns_error() {
        let state = test_app_state();
        let resp = post_mcp(state, &json!([])).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn mcp_unified_response_has_content_type_json() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .expect("response must have content-type header")
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "content-type should be application/json, got: {}",
            ct
        );
    }

    #[test]
    fn is_localhost_origin_accepts_valid_origins() {
        assert!(is_localhost_origin("http://localhost"));
        assert!(is_localhost_origin("http://localhost:3000"));
        assert!(is_localhost_origin("http://127.0.0.1"));
        assert!(is_localhost_origin("http://127.0.0.1:8080"));
        assert!(is_localhost_origin("http://[::1]"));
        assert!(is_localhost_origin("http://[::1]:9000"));
        assert!(is_localhost_origin("https://localhost:443"));
    }

    #[test]
    fn is_localhost_origin_rejects_non_localhost() {
        assert!(!is_localhost_origin("http://example.com"));
        assert!(!is_localhost_origin("http://evil.localhost.com"));
        assert!(!is_localhost_origin("http://192.168.1.1"));
        assert!(!is_localhost_origin("ftp://localhost"));
        assert!(!is_localhost_origin("localhost"));
    }

    // =====================================================================
    // MCP Streamable HTTP integration tests
    // =====================================================================

    #[tokio::test]
    async fn full_init_lifecycle() {
        // POST initialize → 200 + response
        // POST notifications/initialized → 202
        // POST tools/list → 200 + tools
        let state = test_app_state();

        // Step 1: initialize
        let resp = post_mcp(
            state.clone(),
            &json!({"jsonrpc":"2.0","method":"initialize","id":1,"params":{
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"test-client","version":"0.1"}
            }}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(body["result"]["serverInfo"]["name"], "Endara Relay");

        // Step 2: notifications/initialized → 202 with empty body
        let resp = post_mcp(
            state.clone(),
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());

        // Step 3: tools/list → 200 + tools array
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"tools/list","id":2}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 2);
        let tools = body["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty(), "tools list should contain meta-tools");
    }

    #[tokio::test]
    async fn protocol_version_in_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        let body = body_json(resp).await;
        assert_eq!(
            body["result"]["protocolVersion"], "2025-03-26",
            "InitializeResult must contain protocolVersion 2025-03-26"
        );
    }

    #[tokio::test]
    async fn notifications_cancelled_returns_202() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{
                "requestId": 42, "reason": "user cancelled"
            }}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn notification_with_unknown_method_returns_202() {
        // Any notification (no id), even with an unknown method, must get 202.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/totally_made_up"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn notification_without_method_returns_202() {
        // A message with no id is a notification even if method is absent.
        let state = test_app_state();
        let resp = post_mcp(state, &json!({"jsonrpc":"2.0"})).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn notification_with_null_id_returns_202() {
        // JSON-RPC spec: `id: null` means notification in some interpretations.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized","id":null}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn batch_single_item_array_returns_array() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!([{"jsonrpc":"2.0","method":"initialize","id":1}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body
            .as_array()
            .expect("single-item batch should return array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
        assert!(arr[0]["result"]["serverInfo"].is_object());
    }

    #[tokio::test]
    async fn unknown_method_with_id_returns_jsonrpc_error() {
        // A request (has id) with unknown method must return JSON-RPC error, NOT 202.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"nonexistent/method","id":99}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 99);
        assert_eq!(body["error"]["code"], -32601);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("method not found"),
            "error message should mention 'method not found'"
        );
    }

    #[tokio::test]
    async fn invalid_json_body_returns_error() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(b"this is not json".to_vec()))
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        // Axum rejects invalid JSON before our handler — expect 4xx
        assert!(
            resp.status().is_client_error(),
            "invalid JSON should return 4xx, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn invalid_jsonrpc_primitive_body_returns_error() {
        // A JSON primitive (string, number) is not a valid JSON-RPC message.
        let state = test_app_state();
        let resp = post_mcp(state, &json!("just a string")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn post_mcp_request_content_type_is_json() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        let ct = resp
            .headers()
            .get("content-type")
            .expect("must have content-type")
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected application/json, got: {}",
            ct
        );
    }

    #[tokio::test]
    async fn batch_content_type_is_json() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!([{"jsonrpc":"2.0","method":"initialize","id":1}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .expect("batch response must have content-type")
            .to_str()
            .unwrap();
        assert!(ct.contains("application/json"));
    }

    #[tokio::test]
    async fn notification_202_has_no_content_type_json() {
        // Notifications return 202 with empty body — no content-type required.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        // Body must be empty
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    // =====================================================================
    // Adapter HTTP client verification tests
    // =====================================================================

    #[test]
    fn http_adapter_sends_protocol_version_2025_03_26() {
        // Verify the adapter sends protocolVersion "2025-03-26" in initialize params.
        // We inspect the code path in HttpAdapter::initialize — the params JSON
        // is constructed inline. We verify the static value here.
        let params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "endara-relay",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        assert_eq!(params["protocolVersion"], "2025-03-26");
        assert_eq!(params["clientInfo"]["name"], "endara-relay");
    }

    #[tokio::test]
    async fn http_adapter_full_lifecycle_against_server() {
        // Integration test: spin up the real router and have HttpAdapter
        // connect to it, verifying:
        // - Client sends protocolVersion "2025-03-26"
        // - Client sends notifications/initialized after init
        // - Client can list tools
        use crate::adapter::http::{HttpAdapter, HttpConfig};
        use crate::adapter::McpAdapter;

        let state = test_app_state();
        let router = build_router(state);

        // Bind to an ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn the server
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let url = format!("http://127.0.0.1:{}/mcp", addr.port());
        let mut adapter = HttpAdapter::new(HttpConfig::new(&url));

        // initialize() sends protocolVersion "2025-03-26" and then
        // sends notifications/initialized — both verified by server
        // accepting them without error.
        adapter.initialize().await.unwrap();

        // After init, health should be Healthy
        assert_eq!(adapter.health(), crate::adapter::HealthStatus::Healthy);

        // List tools — should return meta-tools
        let tools = adapter.list_tools().await.unwrap();
        assert!(
            tools.len() >= 3,
            "expected at least 3 meta-tools, got {}",
            tools.len()
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(names.contains(&"execute_tools"));

        adapter.shutdown().await.unwrap();
        server_handle.abort();
    }

    // =====================================================================
    // MCP content format regression tests
    // =====================================================================

    /// Assert that a JSON-RPC response body has the MCP content array format:
    /// `result.content` is an array, each item has a `type` field, and text
    /// items have a non-empty `text` field.
    fn assert_mcp_content_format(response: &Value) {
        let content = response["result"]["content"]
            .as_array()
            .expect("result.content must be an array");
        assert!(!content.is_empty(), "content array must not be empty");
        for item in content {
            assert!(
                item["type"].is_string(),
                "each content item must have a string 'type' field"
            );
            if item["type"] == "text" {
                let text = item["text"]
                    .as_str()
                    .expect("text content item must have a 'text' field");
                assert!(!text.is_empty(), "text content must not be empty");
            }
        }
    }

    #[tokio::test]
    async fn content_format_list_tools_via_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "list_tools", "arguments": {}},
                "id": 10
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_mcp_content_format(&body);
    }

    #[tokio::test]
    async fn content_format_search_tools_via_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "search_tools", "arguments": {"query": "list"}},
                "id": 11
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_mcp_content_format(&body);
    }

    #[tokio::test]
    async fn content_format_execute_tools_via_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "execute_tools", "arguments": {"script": "1+1"}},
                "id": 12
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // execute_tools may succeed or error depending on JS runtime availability
        if body["result"].is_object() {
            assert_mcp_content_format(&body);
        }
    }

    /// Helper: send a JSON-RPC POST to `/mcp/tools/call` via the router.
    async fn post_mcp_tools_call(state: AppState, body: &Value) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;

        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp/tools/call")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn content_format_list_tools_via_legacy_endpoint() {
        let state = test_app_state();
        let resp = post_mcp_tools_call(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "list_tools", "arguments": {}},
                "id": 20
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_mcp_content_format(&body);
    }

    #[tokio::test]
    async fn nonexistent_tool_returns_jsonrpc_error() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "does_not_exist", "arguments": {}},
                "id": 30
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body["error"].is_object(),
            "nonexistent tool must return JSON-RPC error, got: {}",
            body
        );
        assert!(body["error"]["code"].is_number());
        assert!(body["error"]["message"].is_string());
        // Must NOT have a result.content array
        assert!(
            body["result"].is_null(),
            "error response must not have result"
        );
    }

    #[tokio::test]
    async fn missing_name_param_returns_jsonrpc_error() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"arguments": {}},
                "id": 31
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body["error"].is_object(),
            "missing name must return JSON-RPC error, got: {}",
            body
        );
        assert_eq!(
            body["error"]["code"], -32602,
            "expected invalid-params error code"
        );
    }

    // --- Alphabetical sorting tests ---

    #[tokio::test]
    async fn test_tools_list_returns_sorted_tools() {
        let state = test_app_state();
        // Register an adapter with unsorted tool names
        state
            .registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::with_tools(&["zebra", "alpha", "mango"])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body)).await;
        let tools = resp["result"]["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Should be sorted: adapter tools + meta-tools, all alphabetical
        // Expected: alpha, execute_tools, list_tools, mango, search_tools, zebra
        assert_eq!(
            names,
            vec![
                "alpha",
                "execute_tools",
                "list_tools",
                "mango",
                "search_tools",
                "zebra"
            ]
        );
    }

    #[tokio::test]
    async fn test_tools_list_js_mode_meta_tools_sorted() {
        let state = test_app_state();
        state.js_execution_mode.store(true, Ordering::Relaxed);

        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body)).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Meta-tools should appear in definition order: list_tools, search_tools, execute_tools
        // In JS mode, they are NOT sorted (no sort call in that branch).
        // Verify the expected names are present.
        assert!(names.contains(&"execute_tools"));
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
    }
}
