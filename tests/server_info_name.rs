//! Integration tests for serverInfo.name enforcement.
//!
//! Tests that adapters reject servers without valid serverInfo.name and
//! enter a Failed state, while valid servers enter Ready state.

mod common;

use common::config::ConfigBuilder;
use common::harness::RelayHarness;
use serde_json::Value;
use std::time::Duration;

fn bad_server_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-bad-server").to_string()
}

/// Helper: poll /api/endpoints until we see a specific lifecycle state for an endpoint.
async fn wait_for_lifecycle_state(
    harness: &RelayHarness,
    endpoint_name: &str,
    expected_state: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/endpoints", harness.base_url());
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_state: Option<String> = None;
    let mut last_body: Option<Value> = None;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Timeout waiting for endpoint '{}' to reach state '{}'. Last state: {:?}, Last body: {:?}",
                endpoint_name, expected_state, last_state, last_body
            ));
        }

        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(body) = resp.json::<Value>().await {
                last_body = Some(body.clone());
                if let Some(endpoints) = body.as_array() {
                    for ep in endpoints {
                        if ep["name"].as_str() == Some(endpoint_name) {
                            last_state = ep["lifecycle"]["state"].as_str().map(|s| s.to_string());
                            if let Some(state) = ep["lifecycle"]["state"].as_str() {
                                if state == expected_state {
                                    return Ok(ep.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Test 1: STDIO adapter with server that omits serverInfo.name → adapter enters Failed state
#[tokio::test]
async fn test_stdio_omit_server_name_enters_failed_state() {
    let config = ConfigBuilder::new()
        .add_stdio("bad-server", &bad_server_bin(), &["--omit-server-name"])
        .build();

    let harness = RelayHarness::start(&config).await;

    // Wait for the endpoint to enter Failed state
    let endpoint =
        wait_for_lifecycle_state(&harness, "bad-server", "Failed", Duration::from_secs(10))
            .await
            .expect("endpoint did not enter Failed state");

    // Verify error details
    let error = &endpoint["lifecycle"]["error"];
    assert_eq!(error["kind"].as_str(), Some("ServerName"));
    assert!(
        error["detail"]
            .as_str()
            .unwrap_or("")
            .contains("did not report"),
        "expected error detail to mention 'did not report', got: {:?}",
        error["detail"]
    );
}

/// Test 2: STDIO adapter with server that returns empty serverInfo.name → Failed state
#[tokio::test]
async fn test_stdio_empty_server_name_enters_failed_state() {
    let config = ConfigBuilder::new()
        .add_stdio("bad-server", &bad_server_bin(), &["--empty-server-name"])
        .build();

    let harness = RelayHarness::start(&config).await;

    // Wait for the endpoint to enter Failed state
    let endpoint =
        wait_for_lifecycle_state(&harness, "bad-server", "Failed", Duration::from_secs(10))
            .await
            .expect("endpoint did not enter Failed state");

    // Verify error details
    let error = &endpoint["lifecycle"]["error"];
    assert_eq!(error["kind"].as_str(), Some("ServerName"));
    assert!(
        error["detail"].as_str().unwrap_or("").contains("empty"),
        "expected error detail to mention 'empty', got: {:?}",
        error["detail"]
    );
}

/// Test 3: STDIO adapter with valid serverInfo.name → adapter enters Ready state
#[tokio::test]
async fn test_stdio_valid_server_name_enters_ready_state() {
    // Use bad-server without flags — it provides a valid serverInfo.name
    let config = ConfigBuilder::new()
        .add_stdio("good-server", &bad_server_bin(), &[])
        .build();

    let harness = RelayHarness::start(&config).await;

    // Wait for the endpoint to enter Ready state
    let endpoint =
        wait_for_lifecycle_state(&harness, "good-server", "Ready", Duration::from_secs(10))
            .await
            .expect("endpoint did not enter Ready state");

    // Verify server_name is present
    assert_eq!(
        endpoint["lifecycle"]["server_name"].as_str(),
        Some("bad-mcp"),
        "expected server_name to be 'bad-mcp'"
    );
}

/// Test 4: Failed adapter contributes zero tools to merged catalog
#[tokio::test]
async fn test_failed_adapter_contributes_zero_tools() {
    let config = ConfigBuilder::new()
        .add_stdio("bad-server", &bad_server_bin(), &["--omit-server-name"])
        .build();

    let harness = RelayHarness::start(&config).await;

    // Wait for the endpoint to enter Failed state
    wait_for_lifecycle_state(&harness, "bad-server", "Failed", Duration::from_secs(10))
        .await
        .expect("endpoint did not enter Failed state");

    // Check the catalog API
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/catalog", harness.base_url()))
        .send()
        .await
        .expect("request failed");

    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let tools = body.as_array().unwrap();

    // Failed adapter should contribute zero tools
    assert!(
        tools.is_empty(),
        "expected zero tools from failed adapter, got {}",
        tools.len()
    );
}

/// Test 5: GET /api/endpoints shows lifecycle.state = "Failed" with error detail for bad servers
#[tokio::test]
async fn test_endpoints_api_shows_failed_with_error_detail() {
    let config = ConfigBuilder::new()
        .add_stdio("bad-server", &bad_server_bin(), &["--omit-server-name"])
        .build();

    let harness = RelayHarness::start(&config).await;

    // Wait for the endpoint to enter Failed state
    let endpoint =
        wait_for_lifecycle_state(&harness, "bad-server", "Failed", Duration::from_secs(10))
            .await
            .expect("endpoint did not enter Failed state");

    // Verify the lifecycle structure
    assert_eq!(endpoint["lifecycle"]["state"].as_str(), Some("Failed"));
    assert!(endpoint["lifecycle"]["error"].is_object());
    assert!(endpoint["lifecycle"]["error"]["kind"].is_string());
    assert!(endpoint["lifecycle"]["error"]["detail"].is_string());
}

/// Test 6: GET /api/endpoints shows lifecycle.state = "Ready" with server_name for good servers
#[tokio::test]
async fn test_endpoints_api_shows_ready_with_server_name() {
    let config = ConfigBuilder::new()
        .add_stdio("good-server", &bad_server_bin(), &[])
        .build();

    let harness = RelayHarness::start(&config).await;

    // Wait for the endpoint to enter Ready state
    let endpoint =
        wait_for_lifecycle_state(&harness, "good-server", "Ready", Duration::from_secs(10))
            .await
            .expect("endpoint did not enter Ready state");

    // Verify the lifecycle structure
    assert_eq!(endpoint["lifecycle"]["state"].as_str(), Some("Ready"));
    assert_eq!(
        endpoint["lifecycle"]["server_name"].as_str(),
        Some("bad-mcp")
    );
}
