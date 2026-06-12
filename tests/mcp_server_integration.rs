use async_trait::async_trait;
use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use endara_relay::config::ProfileConfig;
use endara_relay::js_sandbox::MetaToolHandler;
use endara_relay::profile_registry::ProfileRegistry;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{build_router, start_server, AppState};
use serde_json::{json, Value};
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
        server_type_override: None,
        endpoint_name: "mcp-server-test".into(),
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
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
        profile_registry: Arc::new(ProfileRegistry::new(registry.clone())),
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
        toon_enabled: false,
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
    // 1 catalog tool (unprefixed in single-server mode) + 2 meta-tools = 3 total.
    // execute_tools is gated on local_js_execution and is hidden from the
    // catalog when JS mode is off (this fixture sets js_execution_mode=false).
    assert_eq!(tools.len(), 3);
    // Meta-tools should be present (sans execute_tools)
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"echo"));
    assert!(tool_names.contains(&"list_tools"));
    assert!(tool_names.contains(&"search_tools"));
    assert!(!tool_names.contains(&"execute_tools"));
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

/// Mock adapter whose `call_tool` returns a `CallToolResult` envelope with
/// a JSON-serialized object in its single TextContent entry. Used by the
/// route-level integration tests for the native `tools/call` branch
/// (§5 row 14) to verify TOON gating end-to-end.
struct JsonPayloadAdapter;

#[async_trait]
impl McpAdapter for JsonPayloadAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![ToolInfo {
            name: "json_tool".to_string(),
            description: Some("returns a JSON object in TextContent".to_string()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        }])
    }
    async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, AdapterError> {
        Ok(json!({
            "content": [{
                "type": "text",
                "text": "{\"users\":[{\"id\":1,\"name\":\"a\"},{\"id\":2,\"name\":\"b\"}]}"
            }]
        }))
    }
    fn health(&self) -> HealthStatus {
        HealthStatus::Healthy
    }
    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Build a relay server backed by a single `JsonPayloadAdapter`, with the
/// requested `toon_enabled` flag. Returns the bound HTTP address.
async fn setup_native_toon_server(toon_enabled: bool) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
    registry
        .register(
            "mock-ep".into(),
            Box::new(JsonPayloadAdapter),
            "stdio".into(),
            None,
            None,
        )
        .await;

    let registry_arc = Arc::new(registry.clone());
    let profile_registry = Arc::new(ProfileRegistry::new(registry.clone()));
    let state = AppState {
        registry,
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
        profile_registry,
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
        toon_enabled,
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr)
        .await
        .expect("server start failed");
    (bound_addr, handle)
}

/// §5 row 14 — route-level: the native `tools/call` branch encodes JSON
/// TextContent into TOON when `toon_enabled` is on.
#[tokio::test]
async fn tools_call_native_response_is_toon_when_enabled() {
    let (addr, _handle) = setup_native_toon_server(true).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": { "name": "json_tool", "arguments": {} },
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text field missing");

    // The native branch must have re-encoded the JSON object string as TOON,
    // which is never valid JSON and never starts with `{` or `[`.
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "expected TOON output, got JSON-parseable text: {text}"
    );
    let first = text.chars().next().expect("non-empty TOON output");
    assert!(
        first != '{' && first != '[',
        "expected TOON output (no leading `{{`/`[`), got: {text}"
    );
    // TOON tabular header for the inner `users` array — uniquely identifies
    // TOON output and would not appear in the JSON pass-through.
    assert!(
        text.contains("{id,name}"),
        "expected TOON tabular header `{{id,name}}` in: {text}"
    );
}

/// §5 row 14 sibling — route-level: the native `tools/call` branch leaves
/// the JSON TextContent untouched when `toon_enabled` is off.
#[tokio::test]
async fn tools_call_native_response_is_json_when_disabled() {
    let (addr, _handle) = setup_native_toon_server(false).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/mcp/tools/call", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": { "name": "json_tool", "arguments": {} },
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text field missing");

    // With TOON disabled, the adapter's JSON object string passes through
    // unchanged and must round-trip via `serde_json::from_str`.
    let parsed: Value =
        serde_json::from_str(text).expect("expected JSON pass-through, got non-JSON text");
    assert_eq!(parsed["users"][0]["id"], 1);
    assert_eq!(parsed["users"][0]["name"], "a");
    assert_eq!(parsed["users"][1]["id"], 2);
    assert_eq!(parsed["users"][1]["name"], "b");
}

/// Mock adapter exposing a single, configurable raw tool name. Used by the
/// profile-scoping regression test to register distinct endpoints whose
/// tools are easy to tell apart in a `tools/list` response.
struct NamedToolAdapter {
    tool: &'static str,
}

#[async_trait]
impl McpAdapter for NamedToolAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(vec![ToolInfo {
            name: self.tool.to_string(),
            description: Some(format!("the {} tool", self.tool)),
            input_schema: json!({"type": "object"}),
            annotations: None,
        }])
    }
    async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, AdapterError> {
        Ok(json!({ "content": [{ "type": "text", "text": "ok" }] }))
    }
    fn health(&self) -> HealthStatus {
        HealthStatus::Healthy
    }
    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Regression test for endara-desktop#113: `POST /mcp/{profile}` `tools/list`
/// must return only the tools whose owning endpoint is in the profile's
/// allowlist, plus the `list_tools`/`search_tools` meta-tools — not the full
/// global catalog. Exercises the normal-mode (`js_execution = false`) catalog
/// branch over the real `/mcp/{profile}` serving path.
#[tokio::test]
async fn profile_tools_list_excludes_out_of_profile_tools() {
    let registry = AdapterRegistry::new();
    registry
        .register(
            "gmail".into(),
            Box::new(NamedToolAdapter { tool: "send_email" }),
            "stdio".into(),
            None,
            Some("gmail".into()),
        )
        .await;
    registry
        .register(
            "github".into(),
            Box::new(NamedToolAdapter {
                tool: "list_issues",
            }),
            "stdio".into(),
            None,
            Some("github".into()),
        )
        .await;

    let profile_registry = Arc::new(ProfileRegistry::new(registry.clone()));
    profile_registry
        .rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into()],
            js_execution: false,
            toon_output: false,
        }])
        .await;

    let registry_arc = Arc::new(registry.clone());
    let state = AppState {
        registry,
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
        profile_registry,
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
        toon_enabled: false,
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (addr, _handle) = start_server(router, addr)
        .await
        .expect("server start failed");

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{}/mcp/work", addr))
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 1
        }))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let tools = body["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // In-profile endpoint's tool plus the two normal-mode meta-tools.
    assert!(
        names.contains(&"gmail__send_email"),
        "in-profile tool missing: {names:?}"
    );
    assert!(
        names.contains(&"list_tools"),
        "list_tools missing: {names:?}"
    );
    assert!(
        names.contains(&"search_tools"),
        "search_tools missing: {names:?}"
    );
    // Out-of-profile endpoint's tool must be filtered out.
    assert!(
        !names.contains(&"github__list_issues"),
        "out-of-profile tool leaked into /mcp/work tools/list: {names:?}"
    );
    assert_eq!(
        names.len(),
        3,
        "expected exactly gmail__send_email + 2 meta-tools, got: {names:?}"
    );
}
