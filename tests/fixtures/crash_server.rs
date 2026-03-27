//! Crash MCP server fixture — crashes (exit 1) after N calls.
//! Usage: fixture-crash-server --crash-after 3

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

static CALL_COUNT: AtomicU64 = AtomicU64::new(0);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut crash_after: u64 = 3;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--crash-after" {
            if i + 1 < args.len() {
                crash_after = args[i + 1].parse().unwrap_or(3);
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    eprintln!("crash-server: starting, will crash after {} calls", crash_after);

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("crash-server: read error: {}", e);
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let count = CALL_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        eprintln!("crash-server: call #{}", count);

        if count > crash_after {
            eprintln!("crash-server: crashing after {} calls!", crash_after);
            process::exit(1);
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("crash-server: parse error: {}", e);
                continue;
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = &request["id"];

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "crash-mcp", "version": "0.1.0"}
                },
                "id": id
            }),
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

