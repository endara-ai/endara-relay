//! HTTP MCP server fixture — HTTP server speaking JSON-RPC over POST /mcp.
//! Usage: fixture-http-server --port 9200

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

fn handle_method(method: &str, params: &Value, id: u64) -> Value {
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "http-mcp", "version": "0.1.0"}
            },
            "id": id
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echoes input back",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"]
                        }
                    },
                    {
                        "name": "reverse",
                        "description": "Reverses input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"]
                        }
                    }
                ]
            },
            "id": id
        }),
        "tools/call" => {
            let tool = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let msg = params
                .get("arguments")
                .and_then(|a| a.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let text = match tool {
                "echo" => format!("echo: {}", msg),
                "reverse" => format!("reverse: {}", msg.chars().rev().collect::<String>()),
                _ => format!("unknown tool: {}", tool),
            };
            json!({
                "jsonrpc": "2.0",
                "result": {"content": [{"type": "text", "text": text}]},
                "id": id
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "error": {"code": -32601, "message": "Method not found"},
            "id": id
        }),
    }
}

async fn handle_mcp(Json(body): Json<Value>) -> Json<Value> {
    let method = body["method"].as_str().unwrap_or("");
    let id = body["id"].as_u64().unwrap_or(0);
    let params = body.get("params").cloned().unwrap_or(json!({}));
    eprintln!("http-server: received {} (id={})", method, id);
    Json(handle_method(method, &params, id))
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut port: u16 = 9200;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--port" && i + 1 < args.len() {
            port = args[i + 1].parse().unwrap_or(9200);
            i += 2;
        } else {
            i += 1;
        }
    }

    let app = Router::new().route("/mcp", post(handle_mcp));

    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    eprintln!("http-server: listening on {}", addr);

    axum::serve(listener, app).await.unwrap();
}

