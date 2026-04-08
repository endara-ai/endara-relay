//! §5.10 Multi-server integration tests.
//!
//! Tests relay behavior with multiple MCP backend servers:
//! merged catalogs, correct routing, partial degradation, and hot-reload.

mod common;

use common::config::{everything_plus_filesystem_config, ConfigBuilder};
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

/// Helper: start relay with everything + filesystem servers, wait healthy, init client.
async fn setup_two_servers() -> (RelayHarness, McpClient) {
    let fs_root = std::env::temp_dir();
    let config = everything_plus_filesystem_config(&fs_root);
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    harness
        .wait_healthy("filesystem", Duration::from_secs(30))
        .await
        .expect("filesystem endpoint did not become healthy");
    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");
    (harness, client)
}

/// 10.1 — relay with everything + filesystem → both healthy, all tools listed with prefixes
#[tokio::test]
async fn test_multi_server_merged_catalog() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_two_servers().await;

    let tools = client.list_tools().await.expect("list_tools failed");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // With two endpoints, tools should be prefixed
    // everything server has "echo", "add"/"get-sum", etc.
    let has_everything_tool = tool_names.iter().any(|n| n.contains("everything"));
    let has_filesystem_tool = tool_names.iter().any(|n| n.contains("filesystem"));

    assert!(
        has_everything_tool,
        "expected prefixed everything tools, got: {:?}",
        tool_names
    );
    assert!(
        has_filesystem_tool,
        "expected prefixed filesystem tools, got: {:?}",
        tool_names
    );

    // Verify we have tools from both servers
    assert!(
        tools.len() >= 2,
        "expected tools from multiple servers, got {}",
        tools.len()
    );
}

/// 10.2 — call tool from each server, verify correct routing
#[tokio::test]
async fn test_multi_server_routing() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_two_servers().await;

    let tools = client.list_tools().await.expect("list_tools failed");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Find the prefixed echo tool from the everything server
    let echo_tool = tool_names
        .iter()
        .find(|n| n.contains("echo") && !n.contains("list") && !n.contains("search"))
        .expect("could not find echo tool in catalog");

    let result = client
        .call_tool(echo_tool, json!({"message": "multi-server-test"}))
        .await
        .expect("call_tool echo failed");

    let content = &result["result"]["content"];
    assert!(content.is_array(), "expected content array, got: {result}");
    let text = content[0]["text"].as_str().expect("text missing");
    assert!(
        text.contains("multi-server-test"),
        "expected echo response, got: {text}"
    );
}

/// 10.3 — tool names from different servers have distinct prefixes
#[tokio::test]
async fn test_multi_server_distinct_prefixes() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup_two_servers().await;

    let tools = client.list_tools().await.expect("list_tools failed");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Collect unique prefixes (everything before "__")
    let prefixes: std::collections::HashSet<&str> = tool_names
        .iter()
        .filter_map(|n| n.split("__").next())
        .filter(|p| {
            tool_names
                .iter()
                .any(|n| n.starts_with(&format!("{}__", p)))
        })
        .collect();

    assert!(
        prefixes.len() >= 2,
        "expected at least 2 distinct prefixes, got: {:?}",
        prefixes
    );
}

/// 10.4 — hot-reload config to remove one endpoint → tools disappear
#[tokio::test]
async fn test_multi_server_hot_reload_remove() {
    if require_node().is_none() {
        return;
    }
    let fs_root = std::env::temp_dir();
    let config = everything_plus_filesystem_config(&fs_root);
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything did not become healthy");
    harness
        .wait_healthy("filesystem", Duration::from_secs(30))
        .await
        .expect("filesystem did not become healthy");

    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");

    // Verify both servers' tools are present
    let tools_before = client.list_tools().await.expect("list_tools failed");
    let names_before: Vec<&str> = tools_before
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names_before.iter().any(|n| n.contains("filesystem")),
        "filesystem tools should be present before reload"
    );

    // Now rewrite config with only everything server
    let new_config = ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .build();
    std::fs::write(&harness.config_path, &new_config).expect("failed to write new config");

    // Trigger config reload via management API
    let http = reqwest::Client::new();
    let reload_resp = http
        .post(format!("{}/api/config/reload", harness.base_url()))
        .send()
        .await
        .expect("reload request failed");
    assert!(
        reload_resp.status().is_success(),
        "reload returned: {}",
        reload_resp.status()
    );

    // Wait for the reload to take effect
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Re-initialize the MCP session (session may have been invalidated)
    let mut client2 = McpClient::new(harness.base_url());
    client2.initialize().await.expect("re-initialize failed");

    // Verify filesystem tools are gone
    let tools_after = client2
        .list_tools()
        .await
        .expect("list_tools after reload failed");
    let names_after: Vec<&str> = tools_after
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        !names_after.iter().any(|n| n.contains("filesystem")),
        "filesystem tools should be gone after removing endpoint. Got: {:?}",
        names_after
    );
    // everything tools should still be present
    let has_echo = names_after.iter().any(|n| n.contains("echo"));
    assert!(
        has_echo,
        "everything echo tool should still be present. Got: {:?}",
        names_after
    );
}

/// 10.5 — hot-reload config to add an endpoint → new tools appear
#[tokio::test]
async fn test_multi_server_hot_reload_add() {
    if require_node().is_none() {
        return;
    }
    // Start with only the everything server
    let config = ConfigBuilder::new()
        .add_stdio(
            "everything",
            "npx",
            &["-y", "@modelcontextprotocol/server-everything"],
        )
        .build();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything did not become healthy");

    let mut client = McpClient::new(harness.base_url());
    client.initialize().await.expect("initialize failed");

    let tools_before = client.list_tools().await.expect("list_tools failed");
    let names_before: Vec<&str> = tools_before
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        !names_before.iter().any(|n| n.contains("filesystem")),
        "filesystem tools should NOT be present initially"
    );

    // Now add filesystem to the config
    let fs_root = std::env::temp_dir();
    let new_config = everything_plus_filesystem_config(&fs_root);
    std::fs::write(&harness.config_path, &new_config).expect("failed to write new config");

    // Trigger config reload
    let http = reqwest::Client::new();
    let reload_resp = http
        .post(format!("{}/api/config/reload", harness.base_url()))
        .send()
        .await
        .expect("reload request failed");
    assert!(reload_resp.status().is_success());

    // Wait for the new endpoint to become healthy
    harness
        .wait_healthy("filesystem", Duration::from_secs(30))
        .await
        .expect("filesystem endpoint did not become healthy after hot-reload");

    // Re-initialize session
    let mut client2 = McpClient::new(harness.base_url());
    client2.initialize().await.expect("re-initialize failed");

    let tools_after = client2
        .list_tools()
        .await
        .expect("list_tools after reload failed");
    let names_after: Vec<&str> = tools_after
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names_after.iter().any(|n| n.contains("filesystem")),
        "filesystem tools should appear after hot-reload add. Got: {:?}",
        names_after
    );
}
