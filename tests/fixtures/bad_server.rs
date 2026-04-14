//! Bad MCP server fixture — configurable to omit or invalidate serverInfo.name.
//!
//! Usage:
//!   fixture-bad-server                    # Normal server (has serverInfo.name)
//!   fixture-bad-server --omit-server-name # Omits serverInfo.name entirely
//!   fixture-bad-server --empty-server-name # Returns empty serverInfo.name

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let omit_server_name = args.contains(&"--omit-server-name".to_string());
    let empty_server_name = args.contains(&"--empty-server-name".to_string());

    eprintln!(
        "bad-server: starting (omit={}, empty={})",
        omit_server_name, empty_server_name
    );

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("bad-server: read error: {}", e);
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("bad-server: parse error: {}", e);
                continue;
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = &request["id"];

        let response = match method {
            "initialize" => {
                // Build the result based on flags
                let mut result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}}
                });

                // Add serverInfo based on flags
                if omit_server_name {
                    // Completely omit serverInfo
                    // (or include serverInfo without name)
                    result["serverInfo"] = json!({
                        "version": "0.1.0"
                    });
                } else if empty_server_name {
                    result["serverInfo"] = json!({
                        "name": "",
                        "version": "0.1.0"
                    });
                } else {
                    result["serverInfo"] = json!({
                        "name": "bad-mcp",
                        "version": "0.1.0"
                    });
                }

                json!({
                    "jsonrpc": "2.0",
                    "result": result,
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
                let params = &request["params"];
                let args = &params["arguments"];
                let message = args["message"].as_str().unwrap_or("");
                json!({
                    "jsonrpc": "2.0",
                    "result": {"content": [{"type": "text", "text": format!("echo: {}", message)}]},
                    "id": id
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "error": {"code": -32601, "message": "Method not found"},
                "id": id
            }),
        };

        let mut out = stdout.lock();
        serde_json::to_writer(&mut out, &response).unwrap();
        out.write_all(b"\n").unwrap();
        out.flush().unwrap();
    }
}
