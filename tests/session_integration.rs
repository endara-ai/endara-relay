//! §5.8 Session management integration tests.
//!
//! Verifies that the relay correctly manages MCP sessions.
//! The relay is stateless/permissive — it may or may not return an
//! Mcp-Session-Id header. These tests verify session-like behaviour:
//! successful initialize, subsequent requests, multiple clients, and
//! persistence across requests.

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

/// Helper: spin up relay with everything server (no client init).
async fn setup_harness() -> RelayHarness {
    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    harness
}

/// 8.1 – Initialize response is valid and contains protocol version.
#[tokio::test]
async fn test_session_initialize_response_valid() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let mut client = McpClient::new(harness.base_url());
    let result = client.initialize().await.expect("initialize failed");

    // Must have a valid JSON-RPC response with protocolVersion
    assert_eq!(result["result"]["protocolVersion"], "2025-03-26");
    assert!(
        result["result"]["serverInfo"]["name"].as_str().is_some(),
        "serverInfo.name should be present"
    );
}

/// 8.2 – Subsequent requests after initialize are accepted.
#[tokio::test]
async fn test_session_subsequent_requests_accepted() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");

    // After initialize, subsequent requests should succeed.
    let tools = client
        .list_tools()
        .await
        .expect("list_tools should succeed after initialize");
    assert!(!tools.is_empty(), "Should get tools back");
}

/// 8.3 – Request without prior initialize still works (relay is permissive).
#[tokio::test]
async fn test_session_request_without_initialize_still_works() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;

    // Send a tools/list WITHOUT doing initialize first
    let resp = harness
        .rpc(json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 1
        }))
        .await;

    // Relay is permissive — should return a result
    assert!(
        resp.get("result").is_some(),
        "Request without prior initialize should work (permissive), got: {resp}"
    );
}

/// 8.4 – Different clients can independently initialize and work.
#[tokio::test]
async fn test_session_different_clients_independent() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;

    let mut client1 = McpClient::new(harness.base_url());
    let mut client2 = McpClient::new(harness.base_url());

    client1
        .initialize()
        .await
        .expect("client1 initialize failed");
    client2
        .initialize()
        .await
        .expect("client2 initialize failed");

    // Both clients should independently work
    let tools1 = client1
        .list_tools()
        .await
        .expect("client1 list_tools failed");
    let tools2 = client2
        .list_tools()
        .await
        .expect("client2 list_tools failed");

    assert!(!tools1.is_empty(), "Client 1 should get tools");
    assert!(!tools2.is_empty(), "Client 2 should get tools");
}

/// 8.5 – Session persists across multiple sequential requests.
#[tokio::test]
async fn test_session_persists_across_requests() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");

    // Make multiple sequential requests – all should succeed
    for _ in 0..5 {
        client
            .list_tools()
            .await
            .expect("list_tools should succeed");
    }

    // Also try calling a tool
    let result = client
        .call_tool("echo", json!({"message": "session test"}))
        .await
        .expect("call_tool should succeed within session");
    assert!(
        result.get("result").is_some(),
        "Tool call should have result: {result}"
    );
}
