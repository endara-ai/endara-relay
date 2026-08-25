//! Integration test: JS sandbox execution.
//!
//! Starts relay with echo fixture, uses MetaTools to call execute_tools
//! with a JS script that calls the echo tool, verifies the script result.
//! Also tests list_tools and search_tools meta-tools.

use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::McpAdapter;
use endara_relay::js_sandbox::MetaToolHandler;
use endara_relay::profile_registry::ProfileRegistry;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{
    build_router, start_server, AppState, MetaToolSchemas, SessionIdentityStore,
};
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

async fn setup_js_server(js_mode: bool) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
    let config = StdioConfig {
        command: "bash".to_string(),
        args: vec![echo_script_path().to_string_lossy().to_string()],
        env: HashMap::new(),
        server_type_override: None,
        endpoint_name: "js-test".into(),
        ..Default::default()
    };
    let mut adapter = StdioAdapter::new(config);
    adapter.initialize().await.expect("adapter init failed");
    registry
        .register(
            "echo-ep".into(),
            Box::new(adapter),
            "stdio".into(),
            None,
            Some("echo_ep".into()),
        )
        .await;

    let registry_arc = Arc::new(registry.clone());
    let state = AppState {
        registry: registry.clone(),
        js_execution_mode: Arc::new(AtomicBool::new(js_mode)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
        profile_registry: Arc::new(ProfileRegistry::new(registry.clone())),
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
        toon_enabled: false,
        session_identities: Arc::new(std::sync::Mutex::new(SessionIdentityStore::default())),
        meta_tool_schemas: MetaToolSchemas::new(),
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr, tokio::sync::watch::channel(false).1)
        .await
        .expect("server start failed");
    (bound_addr, handle)
}

#[tokio::test]
async fn test_execute_tools_with_echo() {
    // execute_tools is gated on local_js_execution; enable it for this test.
    let (addr, _handle) = setup_js_server(true).await;
    let client = reqwest::Client::new();

    // Use execute_tools to run a JS script calling the echo tool.
    // The sandbox exposes tools as `tools["prefixed_name"](args)` which
    // returns the parsed JSON result directly (synchronous, no await needed).
    let script = r#"
        var result = tools["echo"]({ message: "from js" });
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
    let (addr, _handle) = setup_js_server(false).await;
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
    // Result should be wrapped in MCP content array format
    let content = body["result"]["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "text");
    let inner: serde_json::Value =
        serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert!(
        inner["total"].as_u64().unwrap() >= 1,
        "expected at least 1 tool"
    );
    let tools = inner["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
}

#[tokio::test]
async fn test_search_tools_meta_tool() {
    let (addr, _handle) = setup_js_server(false).await;
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
    // Result should be wrapped in MCP content array format
    let content = body["result"]["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "text");
    let tools: serde_json::Value =
        serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    let tools = tools.as_array().unwrap();
    assert!(!tools.is_empty(), "search for 'echo' should find tools");
    assert!(
        tools[0]["name"].as_str().unwrap().contains("echo"),
        "first result should contain 'echo'"
    );
}

/// Defense-in-depth: even though `execute_tools` is hidden from the
/// catalog when `local_js_execution` is off, a misbehaving or malicious
/// client could call it directly. The invocation handler must reject it.
#[tokio::test]
async fn test_execute_tools_rejected_when_js_mode_off() {
    let (addr, _handle) = setup_js_server(false).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "execute_tools",
                "arguments": { "script": "return 1;" }
            },
            "id": 99
        }))
        .send()
        .await
        .expect("request failed");

    let body: serde_json::Value = resp.json().await.unwrap();
    let error = body
        .get("error")
        .expect("expected JSON-RPC error when execute_tools is gated off");
    assert_eq!(
        error["code"].as_i64().unwrap(),
        -32601,
        "expected method-not-found code, got: {error}"
    );
    let msg = error["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("execute_tools"),
        "error message should mention execute_tools, got: {msg}"
    );
}
