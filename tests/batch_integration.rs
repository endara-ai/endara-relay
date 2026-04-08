//! §5.6 Batch request integration tests.
//!
//! Verifies that the relay correctly handles JSON-RPC batch requests
//! (arrays of requests sent in a single HTTP POST).

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

/// Helper: spin up relay with everything server, initialize a client.
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

/// 6.1 – Batch with 2+ valid requests returns an array response.
#[tokio::test]
async fn test_batch_multiple_requests_returns_array() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let items = vec![
        json!({"jsonrpc": "2.0", "method": "tools/list", "id": 100}),
        json!({"jsonrpc": "2.0", "method": "tools/list", "id": 101}),
    ];
    let results = client.batch(items).await.expect("batch request failed");
    assert_eq!(
        results.len(),
        2,
        "Expected 2 responses, got {}",
        results.len()
    );

    // Each response should have a matching id
    let ids: Vec<Option<u64>> = results.iter().map(|r| r["id"].as_u64()).collect();
    assert!(ids.contains(&Some(100)), "Missing response for id 100");
    assert!(ids.contains(&Some(101)), "Missing response for id 101");
}

/// 6.2 – Batch with mix of valid and invalid methods returns partial errors.
#[tokio::test]
async fn test_batch_mixed_valid_and_invalid() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let items = vec![
        json!({"jsonrpc": "2.0", "method": "tools/list", "id": 200}),
        json!({"jsonrpc": "2.0", "method": "nonexistent/method", "id": 201}),
    ];
    let results = client.batch(items).await.expect("batch request failed");
    assert_eq!(results.len(), 2, "Expected 2 responses");

    // Find the valid and error responses
    let valid = results
        .iter()
        .find(|r| r["id"].as_u64() == Some(200))
        .unwrap();
    let invalid = results
        .iter()
        .find(|r| r["id"].as_u64() == Some(201))
        .unwrap();

    assert!(
        valid.get("result").is_some(),
        "Valid request should have result: {valid}"
    );
    assert!(
        invalid.get("error").is_some(),
        "Invalid method should have error: {invalid}"
    );
}

/// 6.3 – Batch with notifications mixed in (no id) should work.
#[tokio::test]
async fn test_batch_with_notifications_mixed() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let items = vec![
        json!({"jsonrpc": "2.0", "method": "tools/list", "id": 300}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "method": "tools/list", "id": 301}),
    ];
    let results = client.batch(items).await.expect("batch request failed");

    // Notifications don't get responses, so we should get 2 responses
    assert!(
        results.len() >= 2,
        "Expected at least 2 responses (notifications have no response), got {}",
        results.len()
    );
}

/// 6.4 – Empty batch array returns an error.
#[tokio::test]
async fn test_batch_empty_array_returns_error() {
    if require_node().is_none() {
        return;
    }
    let (harness, _client) = setup().await;

    // Send empty array directly via harness
    let resp = harness.rpc(json!([])).await;

    // JSON-RPC spec says empty batch is an invalid request
    assert!(
        resp.get("error").is_some(),
        "Empty batch should return an error, got: {resp}"
    );
}

/// 6.5 – Large batch (10+ requests) completes successfully.
#[tokio::test]
async fn test_batch_large_completes_successfully() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    let items: Vec<serde_json::Value> = (0..12)
        .map(|i| json!({"jsonrpc": "2.0", "method": "tools/list", "id": 400 + i}))
        .collect();
    let results = client.batch(items).await.expect("large batch failed");

    assert_eq!(
        results.len(),
        12,
        "Expected 12 responses, got {}",
        results.len()
    );
    for result in &results {
        assert!(
            result.get("result").is_some(),
            "Each response should have a result: {result}"
        );
    }
}
