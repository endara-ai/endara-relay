//! §5.5 Notification integration tests.
//!
//! Verifies that the relay correctly handles JSON-RPC notifications
//! (messages without an `id` field).

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

/// 5.1 – Send notifications/initialized, relay accepts with 2xx.
#[tokio::test]
async fn test_notification_initialized_accepted() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    client
        .send_notification("notifications/initialized", json!({}))
        .await
        .expect("notifications/initialized should be accepted");
}

/// 5.2 – Unknown notification method is accepted gracefully.
#[tokio::test]
async fn test_notification_unknown_method_accepted() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    client
        .send_notification("notifications/doesNotExist", json!({}))
        .await
        .expect("unknown notification should be accepted gracefully");
}

/// 5.3 – Notification with params doesn't error.
#[tokio::test]
async fn test_notification_with_params_no_error() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    client
        .send_notification(
            "notifications/initialized",
            json!({"extra": "data", "number": 42}),
        )
        .await
        .expect("notification with params should not error");
}

/// 5.4 – Notification should not return a JSON-RPC result body.
#[tokio::test]
async fn test_notification_returns_no_result() {
    if require_node().is_none() {
        return;
    }
    let (harness, _client) = setup().await;

    let body = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let resp = harness.rpc_raw(body).await;
    let status = resp.status().as_u16();
    assert!(
        status == 202 || status == 200 || status == 204,
        "Expected 2xx for notification, got {status}"
    );

    // Body should be empty or not a JSON-RPC result
    let bytes = resp.bytes().await.unwrap();
    if !bytes.is_empty() {
        let val: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
        // If there's a body, it should not contain a "result" field
        assert!(
            val.get("result").is_none(),
            "Notification should not return a result, got: {val}"
        );
    }
}

/// 5.5 – Multiple rapid notifications don't cause relay errors.
#[tokio::test]
async fn test_multiple_rapid_notifications_no_errors() {
    if require_node().is_none() {
        return;
    }
    let (_harness, client) = setup().await;

    // Fire 10 notifications as fast as possible
    for i in 0..10 {
        let method = if i % 2 == 0 {
            "notifications/initialized"
        } else {
            "notifications/unknown"
        };
        client
            .send_notification(method, json!({"seq": i}))
            .await
            .unwrap_or_else(|e| panic!("Notification {i} failed: {e}"));
    }
}
