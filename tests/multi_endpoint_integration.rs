//! Integration test: Full round-trip with multiple endpoints.
//!
//! Starts relay with 2 different fixture servers (echo via bash script + multi-tool),
//! verifies catalog merges tools from all endpoints with correct prefixes,
//! and routes tool calls to the correct endpoints.

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
use tokio::time::timeout;

fn echo_script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh")
}

fn multi_tool_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-multi-tool-server").to_string()
}

async fn setup_multi_endpoint_server() -> (SocketAddr, AdapterRegistry, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new("testmachine".into());

    // Endpoint 1: echo (bash script)
    let echo_config = StdioConfig {
        command: "bash".to_string(),
        args: vec![echo_script_path().to_string_lossy().to_string()],
        env: HashMap::new(),
    };
    let mut echo_adapter = StdioAdapter::new(echo_config);
    echo_adapter.initialize().await.expect("echo adapter init failed");
    registry.register("echo-ep".into(), Box::new(echo_adapter)).await;

    // Endpoint 2: multi-tool server
    let multi_config = StdioConfig {
        command: multi_tool_bin(),
        args: vec![],
        env: HashMap::new(),
    };
    let mut multi_adapter = StdioAdapter::new(multi_config);
    multi_adapter.initialize().await.expect("multi-tool adapter init failed");
    registry.register("multi-ep".into(), Box::new(multi_adapter)).await;

    let registry_arc = Arc::new(registry.clone());
    let state = AppState {
        registry: registry.clone(),
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr).await.expect("server start failed");

    (bound_addr, registry, handle)
}

#[tokio::test]
async fn test_multi_endpoint_merged_catalog() {
    let (addr, _registry, _handle) = setup_multi_endpoint_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/list", addr))
        .json(&json!({"jsonrpc": "2.0", "method": "tools/list", "id": 1}))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    let tools = body["result"]["tools"].as_array().expect("tools array");

    // echo-ep has 1 tool, multi-ep has 12 tools, + 3 meta-tools = 16 total
    assert_eq!(tools.len(), 16, "expected 16 tools (1+12+3), got {}", tools.len());

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // Verify prefixed names from echo endpoint
    assert!(tool_names.contains(&"testmachine__echo-ep__echo"), "missing echo-ep echo tool");
    // Verify prefixed names from multi-tool endpoint
    assert!(tool_names.contains(&"testmachine__multi-ep__add"), "missing multi-ep add tool");
    assert!(tool_names.contains(&"testmachine__multi-ep__uppercase"), "missing multi-ep uppercase");
    // Verify meta-tools
    assert!(tool_names.contains(&"list_tools"));
    assert!(tool_names.contains(&"search_tools"));
    assert!(tool_names.contains(&"execute_tools"));
}

#[tokio::test]
async fn test_multi_endpoint_cross_routing() {
    let (addr, _registry, _handle) = setup_multi_endpoint_server().await;
    let client = reqwest::Client::new();

    // Call echo tool from echo endpoint
    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0", "method": "tools/call",
            "params": {"name": "testmachine__echo-ep__echo", "arguments": {"message": "cross-route"}},
            "id": 2
        }))
        .send().await.expect("request failed");
    let body: serde_json::Value = resp.json().await.unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("cross-route"));

    // Call add tool from multi-tool endpoint
    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0", "method": "tools/call",
            "params": {"name": "testmachine__multi-ep__add", "arguments": {"a": 3, "b": 7}},
            "id": 3
        }))
        .send().await.expect("request failed");
    let body: serde_json::Value = resp.json().await.unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("10"), "expected 10, got: {}", text);

    // Call uppercase tool from multi-tool endpoint
    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0", "method": "tools/call",
            "params": {"name": "testmachine__multi-ep__uppercase", "arguments": {"text": "hello"}},
            "id": 4
        }))
        .send().await.expect("request failed");
    let body: serde_json::Value = resp.json().await.unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "HELLO");
}

