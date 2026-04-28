//! Test fixture: stdio MCP server that supports tool-list swapping with
//! `notifications/tools/list_changed`, plus a writable `tools/list` call
//! counter for cache-behaviour assertions.
//!
//! Behaviour:
//! - On startup, advertises tools `swap_tools` and `get_status`.
//! - On `tools/call swap_tools`: flips internal state to "swapped",
//!   appends a `notifications/tools/list_changed` line to stdout *before*
//!   the tool-call response, then returns a small text result.
//! - After the swap, `tools/list` returns three tools (the original two
//!   plus `extra_tool_after_swap`).
//! - Every `tools/list` increments a persistent counter stored at the path
//!   given by the env var `FIXTURE_LIST_COUNT_FILE` (when set). The counter
//!   is seeded from the file on startup so it survives process restarts —
//!   integration tests that disable/enable the endpoint can keep using a
//!   single monotonic counter for assertions.
//!
//! Usage: `fixture-swap-notify-server`

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

fn initial_tools() -> Value {
    json!([
        {"name": "swap_tools",
         "description": "Trigger a tool-list swap and emit list_changed",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "get_status",
         "description": "Returns whether the swap has happened",
         "inputSchema": {"type": "object", "properties": {}}}
    ])
}

fn swapped_tools() -> Value {
    json!([
        {"name": "swap_tools",
         "description": "Trigger a tool-list swap and emit list_changed",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "get_status",
         "description": "Returns whether the swap has happened",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "extra_tool_after_swap",
         "description": "Tool that only appears after swap_tools is invoked",
         "inputSchema": {"type": "object", "properties": {}}}
    ])
}

fn write_count(path: &Option<String>, value: u64) {
    if let Some(p) = path {
        let _ = std::fs::write(p, value.to_string());
    }
}

fn main() {
    eprintln!("swap-notify-server: starting");
    let count_file = std::env::var("FIXTURE_LIST_COUNT_FILE").ok();
    // Seed the counter from the file (if any) so disable→enable restarts
    // observe a single monotonic counter across the whole test.
    let initial_count: u64 = count_file
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let list_count = AtomicU64::new(initial_count);
    let swapped = AtomicBool::new(false);

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("parse error: {}", e);
                continue;
            }
        };
        let method = request["method"].as_str().unwrap_or("");
        let id = &request["id"];

        let response: Option<Value> = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": "swap-notify-mcp", "version": "0.1.0"}
                },
                "id": id
            })),
            "tools/list" => {
                let new_count = list_count.fetch_add(1, Ordering::SeqCst) + 1;
                write_count(&count_file, new_count);
                let tools = if swapped.load(Ordering::SeqCst) {
                    swapped_tools()
                } else {
                    initial_tools()
                };
                Some(json!({
                    "jsonrpc": "2.0",
                    "result": {"tools": tools},
                    "id": id
                }))
            }
            "tools/call" => {
                let p = &request["params"];
                let name = p["name"].as_str().unwrap_or("");
                let text = match name {
                    "swap_tools" => {
                        swapped.store(true, Ordering::SeqCst);
                        // Emit list_changed BEFORE returning the call response so
                        // the relay observes the notification first.
                        let mut out = stdout.lock();
                        let notif = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/tools/list_changed"
                        });
                        serde_json::to_writer(&mut out, &notif).unwrap();
                        out.write_all(b"\n").unwrap();
                        out.flush().unwrap();
                        drop(out);
                        "swap triggered".to_string()
                    }
                    "get_status" => {
                        if swapped.load(Ordering::SeqCst) {
                            "swapped".to_string()
                        } else {
                            "initial".to_string()
                        }
                    }
                    "extra_tool_after_swap" => "extra".to_string(),
                    other => format!("unknown tool: {}", other),
                };
                Some(json!({
                    "jsonrpc": "2.0",
                    "result": {"content": [{"type": "text", "text": text}]},
                    "id": id
                }))
            }
            _ => {
                if id.is_null() {
                    None
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "error": {"code": -32601, "message": "Method not found"},
                        "id": id
                    }))
                }
            }
        };

        if let Some(response) = response {
            let mut out = stdout.lock();
            serde_json::to_writer(&mut out, &response).unwrap();
            out.write_all(b"\n").unwrap();
            out.flush().unwrap();
        }
    }
}
