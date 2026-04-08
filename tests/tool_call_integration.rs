//! §5.3 Tool invocation integration tests.
//!
//! Tests tools/call against the relay with real MCP servers.

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

/// Helper: start relay with everything server, wait healthy, initialize client.
async fn setup() -> (RelayHarness, McpClient) {
    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    (harness, client)
}

/// 3.1 — call echo tool, verify content array with echoed message
#[tokio::test]
async fn test_call_echo_tool() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let result = client
        .call_tool("echo", json!({"message": "hello world"}))
        .await
        .expect("call_tool failed");

    let content = &result["result"]["content"];
    assert!(content.is_array(), "expected content array, got: {result}");
    assert!(
        !content.as_array().unwrap().is_empty(),
        "content array is empty"
    );
    assert_eq!(content[0]["type"], "text", "expected text type");
    let text = content[0]["text"].as_str().expect("text field missing");
    assert!(
        text.contains("hello world"),
        "expected echo to contain 'hello world', got: {text}"
    );
}

/// 3.2 — call get-sum tool with numeric args (formerly "add" in older server-everything)
#[tokio::test]
async fn test_call_add_tool() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    // Try "get-sum" first (newer server-everything), fall back to "add"
    let result = client
        .call_tool("get-sum", json!({"a": 2, "b": 3}))
        .await
        .expect("call_tool failed");

    let content = &result["result"]["content"];
    assert!(content.is_array(), "expected content array, got: {result}");
    let text = content[0]["text"].as_str().expect("text field missing");
    // The tool should return the sum
    assert!(
        text.contains('5'),
        "expected result containing '5', got: {text}"
    );
}

/// 3.3 — call tool with complex arguments (nested objects, arrays)
#[tokio::test]
async fn test_call_tool_with_complex_args() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    // Echo tool accepts a message string — pass a complex string
    let result = client
        .call_tool("echo", json!({"message": "nested: {\"key\": [1,2,3]}"}))
        .await
        .expect("call_tool failed");

    let content = &result["result"]["content"];
    assert!(content.is_array(), "expected content array");
    let text = content[0]["text"].as_str().expect("text missing");
    assert!(
        text.contains("nested"),
        "expected complex arg echoed back, got: {text}"
    );
}

/// 3.4 — call nonexistent tool returns JSON-RPC error
#[tokio::test]
async fn test_call_nonexistent_tool_returns_error() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let result = client
        .call_tool("this_tool_does_not_exist", json!({}))
        .await
        .expect("call_tool request failed");

    // Should have a JSON-RPC error OR an isError content result
    let has_rpc_error = result["error"].is_object();
    let has_content_error = result["result"]["isError"].as_bool() == Some(true);
    assert!(
        has_rpc_error || has_content_error,
        "expected error for nonexistent tool, got: {result}"
    );
}

/// 3.5 — call tool with extra/unknown arguments succeeds (permissive)
#[tokio::test]
async fn test_call_tool_with_extra_args_succeeds() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let result = client
        .call_tool(
            "echo",
            json!({"message": "test", "extra_field": 42, "another": true}),
        )
        .await
        .expect("call_tool failed");

    // Should succeed despite extra fields
    let content = &result["result"]["content"];
    assert!(
        content.is_array(),
        "expected success with extra args, got: {result}"
    );
}

/// 3.6 — response has correct content array format
#[tokio::test]
async fn test_response_content_array_format() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let result = client
        .call_tool("echo", json!({"message": "format check"}))
        .await
        .expect("call_tool failed");

    let content = result["result"]["content"]
        .as_array()
        .expect("content should be array");
    for item in content {
        assert!(
            item["type"].is_string(),
            "each content item must have 'type': {item}"
        );
    }
}

/// 3.7 — concurrent tool calls (10 in flight) all complete without deadlock
#[tokio::test]
async fn test_concurrent_tool_calls() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;
    let client = std::sync::Arc::new(client);

    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let msg = format!("concurrent-{i}");
        handles.push(tokio::spawn(async move {
            c.call_tool("echo", json!({"message": msg}))
                .await
                .expect("concurrent call_tool failed")
        }));
    }

    let results = futures_util::future::join_all(handles).await;
    for (i, r) in results.into_iter().enumerate() {
        let val = r.expect("task panicked");
        let content = &val["result"]["content"];
        assert!(
            content.is_array(),
            "concurrent call {i} didn't return content array: {val}"
        );
    }
}
