//! Smoke test: spawns the relay binary with the everything MCP server
//! and verifies that tools/list returns the expected tools.

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn test_harness_smoke_tools_list() {
    if require_node().is_none() {
        return;
    }

    let config = everything_config();
    let harness = RelayHarness::start(&config).await;

    // Wait for the everything endpoint to become healthy
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");

    // Initialize the MCP session
    let mut client = McpClient::new(harness.base_url());
    let init_result = client.initialize().await.expect("initialize failed");

    // Verify initialize response
    assert_eq!(init_result["result"]["protocolVersion"], "2025-03-26");
    assert!(init_result["result"]["serverInfo"]["name"]
        .as_str()
        .is_some());

    // List tools
    let tools = client.list_tools().await.expect("list_tools failed");
    assert!(
        !tools.is_empty(),
        "Expected at least one tool, got empty list"
    );

    // The everything server exposes tools like echo, add, etc.
    // With a single endpoint, tools should not be prefixed.
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Check that "echo" is present (it's always in server-everything)
    assert!(
        tool_names.contains(&"echo"),
        "Expected 'echo' tool in list, got: {:?}",
        tool_names
    );
}

#[tokio::test]
async fn test_harness_smoke_echo_tool() {
    if require_node().is_none() {
        return;
    }

    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");

    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");

    // Call the echo tool
    let result = client
        .call_tool("echo", json!({"message": "hello from smoke test"}))
        .await
        .expect("call_tool failed");

    // Verify the response has content with the echoed message
    let content = &result["result"]["content"];
    assert!(
        content.is_array(),
        "Expected content array, got: {}",
        result
    );
    let text = content[0]["text"]
        .as_str()
        .expect("expected text in content");
    assert!(
        text.contains("hello from smoke test"),
        "Expected echo response to contain our message, got: {}",
        text
    );
}

#[tokio::test]
async fn test_harness_smoke_rpc_raw() {
    if require_node().is_none() {
        return;
    }

    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");

    // Use the raw rpc method to verify status code and headers
    let body = json!({
      "jsonrpc": "2.0",
      "method": "initialize",
      "params": {
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": { "name": "test", "version": "0.1" }
      },
      "id": 1
    });

    let resp = harness.rpc_raw(body).await;
    assert!(
        resp.status().is_success(),
        "Expected success status, got: {}",
        resp.status()
    );
}
