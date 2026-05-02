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

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh")
}

async fn setup_server() -> (SocketAddr, AdapterRegistry, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();

    let config = StdioConfig {
        command: "bash".to_string(),
        args: vec![fixture_path().to_string_lossy().to_string()],
        env: HashMap::new(),
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
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
    };
    let router = build_router(state);
    // Bind to port 0 to get a random available port
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr)
        .await
        .expect("server start failed");

    (bound_addr, registry, handle)
}

#[tokio::test]
async fn test_mcp_initialize() {
    let (addr, _registry, _handle) = setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/initialize", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            },
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    assert!(body["result"]["serverInfo"]["name"].as_str().is_some());
}

#[tokio::test]
async fn test_mcp_tools_list_prefixed() {
    let (addr, _registry, _handle) = setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/list", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 2
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    let tools = body["result"]["tools"].as_array().expect("tools array");
    // 1 catalog tool (unprefixed in single-server mode) + 3 meta-tools = 4 total
    assert_eq!(tools.len(), 4);
    // With single active endpoint, tool names are not prefixed
    assert_eq!(tools[0]["name"], "echo");
    assert!(tools[0]["description"].as_str().is_some());
    assert!(tools[0]["inputSchema"].is_object());
    // Meta-tools should be present
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"list_tools"));
    assert!(tool_names.contains(&"search_tools"));
    assert!(tool_names.contains(&"execute_tools"));
}

#[tokio::test]
async fn test_mcp_tools_call_routing() {
    let (addr, _registry, _handle) = setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": { "message": "hello from test" }
            },
            "id": 3
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    let content = &body["result"]["content"];
    let text = content[0]["text"].as_str().expect("text field");
    assert!(
        text.contains("hello from test"),
        "expected echo response, got: {}",
        text
    );
}

#[tokio::test]
async fn test_mcp_tools_call_invalid_prefix() {
    let (addr, _registry, _handle) = setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "nonexistent_tool",
                "arguments": {}
            },
            "id": 4
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].is_object(), "expected error response");
}
