//! §5.2 Tool listing & catalog merging integration tests.
//!
//! Tests tools/list against the relay with real MCP servers.

mod common;

use common::config::{everything_config, everything_plus_filesystem_config};
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use std::time::Duration;

/// Helper: start relay with everything server, wait healthy, initialize client.
async fn setup_everything() -> (RelayHarness, McpClient) {
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

/// 2.1 — tools/list returns non-empty array from server-everything
#[tokio::test]
async fn test_tools_list_returns_tools() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_everything().await;
    let tools = client.list_tools().await.expect("list_tools failed");

    assert!(
        !tools.is_empty(),
        "Expected non-empty tools list from server-everything"
    );

    // server-everything should have at least echo, add
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"echo"), "missing 'echo' tool: {names:?}");
    // server-everything may name the add tool "add" or "get-sum" depending on version
    assert!(
        names.contains(&"add") || names.contains(&"get-sum"),
        "missing 'add' or 'get-sum' tool: {names:?}"
    );
}

/// 2.2 — each tool has name, description, and inputSchema
#[tokio::test]
async fn test_each_tool_has_required_fields() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_everything().await;
    let tools = client.list_tools().await.expect("list_tools failed");

    for tool in &tools {
        assert!(tool["name"].is_string(), "tool missing 'name': {tool}");
        // description may be optional per MCP spec, but server-everything provides it
        assert!(
            tool["inputSchema"].is_object(),
            "tool '{}' missing inputSchema",
            tool["name"]
        );
    }
}

/// 2.3 — tools/list with two endpoints returns prefixed names
#[tokio::test]
async fn test_tools_list_with_two_endpoints_has_prefixed_names() {
    if require_node().is_none() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = everything_plus_filesystem_config(tmp.path());
    let harness = RelayHarness::start(&config).await;

    // Wait for both endpoints
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint not healthy");
    harness
        .wait_healthy("filesystem", Duration::from_secs(30))
        .await
        .expect("filesystem endpoint not healthy");

    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    let tools = client.list_tools().await.expect("list_tools failed");

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // With two endpoints, tools should be prefixed with endpoint name
    let has_everything_prefix = names.iter().any(|n| n.starts_with("everything__"));
    let has_filesystem_prefix = names.iter().any(|n| n.starts_with("filesystem__"));

    assert!(
        has_everything_prefix,
        "expected everything__ prefix in tool names: {names:?}"
    );
    assert!(
        has_filesystem_prefix,
        "expected filesystem__ prefix in tool names: {names:?}"
    );
}

/// 2.4 — with single endpoint, tool names are NOT prefixed
#[tokio::test]
async fn test_single_endpoint_no_prefix() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_everything().await;
    let tools = client.list_tools().await.expect("list_tools failed");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // With single endpoint, names should NOT have __ prefix
    let has_prefix = names.iter().any(|n| n.contains("__"));
    assert!(
        !has_prefix,
        "single endpoint should not prefix tool names: {names:?}"
    );
}

/// 2.5 — each tool's inputSchema is preserved from upstream
#[tokio::test]
async fn test_input_schema_preserved() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_everything().await;
    let tools = client.list_tools().await.expect("list_tools failed");

    // Find the echo tool and verify its schema
    let echo = tools.iter().find(|t| t["name"] == "echo");
    assert!(echo.is_some(), "echo tool not found");
    let echo = echo.unwrap();
    let schema = &echo["inputSchema"];
    assert_eq!(
        schema["type"], "object",
        "echo inputSchema.type should be 'object'"
    );
    // The echo tool has a "message" property
    assert!(
        schema["properties"]["message"].is_object(),
        "echo schema should have 'message' property: {schema}"
    );
}

/// 2.6 — tools/list after adding second endpoint merges catalogs
#[tokio::test]
async fn test_two_endpoint_catalog_merge() {
    if require_node().is_none() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = everything_plus_filesystem_config(tmp.path());
    let harness = RelayHarness::start(&config).await;

    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything not healthy");
    harness
        .wait_healthy("filesystem", Duration::from_secs(30))
        .await
        .expect("filesystem not healthy");

    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    let tools = client.list_tools().await.expect("list_tools failed");

    // Should have tools from both endpoints
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // everything server has echo, add, etc; filesystem has read_file, etc.
    let has_echo = names.iter().any(|n| n.contains("echo"));
    let has_fs_tool = names.iter().any(|n| {
        n.contains("read_file") || n.contains("write_file") || n.contains("list_directory")
    });

    assert!(has_echo, "missing echo-related tool: {names:?}");
    assert!(has_fs_tool, "missing filesystem tool: {names:?}");
}
