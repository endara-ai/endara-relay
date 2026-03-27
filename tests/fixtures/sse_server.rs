//! SSE MCP server fixture — HTTP server speaking SSE MCP protocol.
//! Usage: fixture-sse-server --port 9100
//!
//! Protocol: GET /sse returns SSE stream. First event is "endpoint" with POST URL.
//! Client POSTs JSON-RPC to that URL, responses come as "message" SSE events.

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
struct AppState {
    tx: Arc<broadcast::Sender<(u64, Value)>>,
    next_session: Arc<Mutex<u64>>,
}

async fn handle_sse(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.tx.subscribe();
    let session_id = {
        let mut id = state.next_session.lock().await;
        let s = *id;
        *id += 1;
        s
    };

    let (tx, client_rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);

    // Send endpoint event
    let tx_clone = tx.clone();
    tokio::spawn(async move {
        let _ = tx_clone
            .send(Ok(Event::default().event("endpoint").data("/message")))
            .await;
    });

    // Forward matching responses
    let mut stream = BroadcastStream::new(rx);
    tokio::spawn(async move {
        while let Some(Ok((id, response))) = stream.next().await {
            let _ = id; // All sessions get all messages; real impl would filter
            let data = serde_json::to_string(&response).unwrap_or_default();
            if tx.send(Ok(Event::default().event("message").data(data))).await.is_err() {
                break;
            }
        }
    });

    eprintln!("sse-server: new SSE connection (session {})", session_id);
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(client_rx)).keep_alive(KeepAlive::default())
}

fn handle_method(method: &str, params: &Value, id: u64) -> Value {
    match method {
        "initialize" => json!({"jsonrpc": "2.0", "result": {
            "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
            "serverInfo": {"name": "sse-mcp", "version": "0.1.0"}}, "id": id}),
        "tools/list" => json!({"jsonrpc": "2.0", "result": {"tools": [
            {"name": "echo", "description": "Echoes input back",
             "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}},
            {"name": "reverse", "description": "Reverses input",
             "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}}
        ]}, "id": id}),
        "tools/call" => {
            let tool = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let msg = params.get("arguments").and_then(|a| a.get("message")).and_then(|v| v.as_str()).unwrap_or("");
            let text = match tool {
                "echo" => format!("echo: {}", msg),
                "reverse" => format!("reverse: {}", msg.chars().rev().collect::<String>()),
                _ => format!("unknown tool: {}", tool),
            };
            json!({"jsonrpc": "2.0", "result": {"content": [{"type": "text", "text": text}]}, "id": id})
        }
        _ => json!({"jsonrpc": "2.0", "error": {"code": -32601, "message": "Method not found"}, "id": id}),
    }
}

async fn handle_message(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let method = body["method"].as_str().unwrap_or("");
    let id = body["id"].as_u64().unwrap_or(0);
    let params = body.get("params").cloned().unwrap_or(json!({}));
    eprintln!("sse-server: received {} (id={})", method, id);

    let response = handle_method(method, &params, id);
    // Broadcast response to SSE clients
    let _ = state.tx.send((id, response.clone()));
    Json(response)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut port: u16 = 9100;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--port" && i + 1 < args.len() {
            port = args[i + 1].parse().unwrap_or(9100);
            i += 2;
        } else { i += 1; }
    }

    let (tx, _) = broadcast::channel::<(u64, Value)>(256);
    let state = AppState { tx: Arc::new(tx), next_session: Arc::new(Mutex::new(0)) };

    let app = Router::new()
        .route("/sse", get(handle_sse))
        .route("/message", post(handle_message))
        .with_state(state);

    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    eprintln!("sse-server: listening on {}", addr);

    axum::serve(listener, app).await.unwrap();
}

