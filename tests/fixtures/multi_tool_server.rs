//! Multi-tool MCP server fixture — exposes 12 tools with varied input schemas.
//! Usage: fixture-multi-tool-server

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn tool_definitions() -> Value {
    json!([
        {"name": "echo", "description": "Echoes a message",
         "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}},
        {"name": "add", "description": "Adds two numbers",
         "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
        {"name": "concat", "description": "Concatenates strings",
         "inputSchema": {"type": "object", "properties": {"items": {"type": "array", "items": {"type": "string"}}, "separator": {"type": "string"}}, "required": ["items"]}},
        {"name": "lookup", "description": "Looks up a key in an object",
         "inputSchema": {"type": "object", "properties": {"data": {"type": "object"}, "key": {"type": "string"}}, "required": ["data", "key"]}},
        {"name": "toggle", "description": "Negates a boolean",
         "inputSchema": {"type": "object", "properties": {"value": {"type": "boolean"}}, "required": ["value"]}},
        {"name": "uppercase", "description": "Converts text to uppercase",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "count_items", "description": "Counts items in an array",
         "inputSchema": {"type": "object", "properties": {"items": {"type": "array"}}, "required": ["items"]}},
        {"name": "format_greeting", "description": "Formats a greeting with optional title",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "title": {"type": "string"}, "formal": {"type": "boolean"}}, "required": ["name"]}},
        {"name": "multiply_matrix", "description": "Multiplies 2x2 matrix by scalar",
         "inputSchema": {"type": "object", "properties": {"matrix": {"type": "array", "items": {"type": "array", "items": {"type": "number"}}}, "scalar": {"type": "number"}}, "required": ["matrix", "scalar"]}},
        {"name": "get_metadata", "description": "Returns server metadata",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "validate_email", "description": "Validates email format",
         "inputSchema": {"type": "object", "properties": {"email": {"type": "string"}}, "required": ["email"]}},
        {"name": "merge_objects", "description": "Merges two JSON objects",
         "inputSchema": {"type": "object", "properties": {"base": {"type": "object"}, "overlay": {"type": "object"}}, "required": ["base", "overlay"]}}
    ])
}

fn handle_tool_call(name: &str, args: &Value) -> String {
    match name {
        "echo" => format!("echo: {}", args["message"].as_str().unwrap_or("")),
        "add" => {
            let a = args["a"].as_f64().unwrap_or(0.0);
            let b = args["b"].as_f64().unwrap_or(0.0);
            format!("{}", a + b)
        }
        "concat" => {
            let items: Vec<&str> = args["items"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            items.join(args["separator"].as_str().unwrap_or(","))
        }
        "lookup" => {
            let key = args["key"].as_str().unwrap_or("");
            args["data"].get(key).map(|v| v.to_string()).unwrap_or_else(|| "null".to_string())
        }
        "toggle" => format!("{}", !args["value"].as_bool().unwrap_or(false)),
        "uppercase" => args["text"].as_str().unwrap_or("").to_uppercase(),
        "count_items" => format!("{}", args["items"].as_array().map(|a| a.len()).unwrap_or(0)),
        "format_greeting" => {
            let name = args["name"].as_str().unwrap_or("World");
            let title = args["title"].as_str();
            if args["formal"].as_bool().unwrap_or(false) {
                format!("Dear {}{}", title.map(|t| format!("{} ", t)).unwrap_or_default(), name)
            } else {
                format!("Hello, {}!", name)
            }
        }
        "multiply_matrix" => {
            let s = args["scalar"].as_f64().unwrap_or(1.0);
            if let Some(matrix) = args["matrix"].as_array() {
                let rows: Vec<String> = matrix.iter().map(|row| {
                    let vals: Vec<String> = row.as_array().unwrap_or(&vec![]).iter()
                        .map(|v| format!("{}", v.as_f64().unwrap_or(0.0) * s)).collect();
                    format!("[{}]", vals.join(","))
                }).collect();
                format!("[{}]", rows.join(","))
            } else { "[]".to_string() }
        }
        "get_metadata" => json!({"server": "multi-tool-mcp", "version": "0.1.0", "tool_count": 12}).to_string(),
        "validate_email" => {
            let e = args["email"].as_str().unwrap_or("");
            format!("{}", e.contains('@') && e.contains('.'))
        }
        "merge_objects" => {
            let mut base = args["base"].clone();
            if let (Some(b), Some(o)) = (base.as_object_mut(), args["overlay"].as_object()) {
                for (k, v) in o { b.insert(k.clone(), v.clone()); }
            }
            base.to_string()
        }
        _ => format!("unknown tool: {}", name),
    }
}

fn main() {
    eprintln!("multi-tool-server: starting with 12 tools");
    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        if line.trim().is_empty() { continue; }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v, Err(e) => { eprintln!("parse error: {}", e); continue; }
        };
        let method = request["method"].as_str().unwrap_or("");
        let id = &request["id"];

        let response = match method {
            "initialize" => json!({"jsonrpc": "2.0", "result": {
                "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                "serverInfo": {"name": "multi-tool-mcp", "version": "0.1.0"}}, "id": id}),
            "tools/list" => json!({"jsonrpc": "2.0", "result": {"tools": tool_definitions()}, "id": id}),
            "tools/call" => {
                let p = &request["params"];
                let text = handle_tool_call(p["name"].as_str().unwrap_or(""), &p["arguments"]);
                json!({"jsonrpc": "2.0", "result": {"content": [{"type": "text", "text": text}]}, "id": id})
            }
            _ => json!({"jsonrpc": "2.0", "error": {"code": -32601, "message": "Method not found"}, "id": id}),
        };

        let mut out = stdout.lock();
        serde_json::to_writer(&mut out, &response).unwrap();
        out.write_all(b"\n").unwrap();
        out.flush().unwrap();
    }
}

