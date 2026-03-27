//! Slow MCP server fixture — same as echo but adds configurable delay.
//! Usage: fixture-slow-server --delay-ms 500

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut delay_ms: u64 = 100;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--delay-ms" {
            if i + 1 < args.len() {
                delay_ms = args[i + 1].parse().unwrap_or(100);
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    eprintln!("slow-server: starting with delay_ms={}", delay_ms);

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("slow-server: read error: {}", e);
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("slow-server: parse error: {}", e);
                continue;
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = &request["id"];

        // Add delay before responding
        thread::sleep(Duration::from_millis(delay_ms));

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "slow-mcp", "version": "0.1.0"}
                },
                "id": id
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echoes input back (slowly)",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"]
                            }
                        },
                        {
                            "name": "reverse",
                            "description": "Reverses input (slowly)",
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
                let params = &request["params"];
                let tool_name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];
                let message = args["message"].as_str().unwrap_or("");

                let text = match tool_name {
                    "echo" => format!("echo: {}", message),
                    "reverse" => format!("reverse: {}", message.chars().rev().collect::<String>()),
                    _ => format!("unknown tool: {}", tool_name),
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
        };

        let mut out = stdout.lock();
        serde_json::to_writer(&mut out, &response).unwrap();
        out.write_all(b"\n").unwrap();
        out.flush().unwrap();
        eprintln!(
            "slow-server: responded to {} (delayed {}ms)",
            method, delay_ms
        );
    }
}
