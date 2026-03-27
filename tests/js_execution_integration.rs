//! Integration test: JS sandbox execution.
//!
//! Starts relay with echo fixture, uses MetaTools to call execute_tools
//! with a JS script that calls the echo tool, verifies the script result.
//! Also tests list_tools and search_tools meta-tools.

use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::McpAdapter;
use endara_relay::js_sandbox::MetaToolHandler;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{build_router, start_server, AppState};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

fn echo_script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh")
}

async fn setup_js_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new("m".into());
    let config = StdioConfig {
        command: "bash".to_string(),
        args: vec![echo_script_path().to_string_lossy().to_string()],
        env: HashMap::new(),
    };
    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("adapter init failed");
    registry.register("echo-ep".into(), Box::new(adapter)).await;

    let registry_arc = Arc::new(registry.clone());
    let state = AppState {
        registry: registry.clone(),
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr).await.expect("server start failed");
    (bound_addr, handle)
}

#[tokio::test]
async fn test_execute_tools_with_echo() {
    let (addr, _handle) = setup_js_server().await;
    let client = reqwest::Client::new();

    // Use execute_tools to run a JS script calling the echo tool.
    // The sandbox exposes tools as `tools["prefixed_name"](args)` which
    // returns the parsed JSON result directly (synchronous, no await needed).
    let script = r#"
        var result = tools["m__echo-ep__echo"]({ message: "from js" });
        return result;
    "#;

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "execute_tools",
                "arguments": { "script": script }
            },
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    // The result should contain the echo response
    let result = &body["result"];
    let result_str = serde_json::to_string(result).unwrap();
    assert!(
        result_str.contains("from js"),
        "expected JS result to contain echo response, got: {}",
        result_str
    );
}

#[tokio::test]
async fn test_list_tools_meta_tool() {
    let (addr, _handle) = setup_js_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": { "name": "list_tools", "arguments": {} },
            "id": 2
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    let result = &body["result"];
    assert!(result["total"].as_u64().unwrap() >= 1, "expected at least 1 tool");
    let tools = result["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
}

#[tokio::test]
async fn test_search_tools_meta_tool() {
    let (addr, _handle) = setup_js_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": { "name": "search_tools", "arguments": { "query": "echo" } },
            "id": 3
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    let result = &body["result"];
    let tools = result.as_array().unwrap();
    assert!(!tools.is_empty(), "search for 'echo' should find tools");
    assert!(
        tools[0]["name"].as_str().unwrap().contains("echo"),
        "first result should contain 'echo'"
    );
}

