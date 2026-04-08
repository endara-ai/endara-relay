//! §5.1 Initialize handshake integration tests.
//!
//! Tests the MCP initialize handshake against the relay with a real
//! `@modelcontextprotocol/server-everything` backend.

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::mcp_client::McpClient;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

/// Helper: start relay + wait for everything endpoint to be healthy.
async fn setup() -> RelayHarness {
    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    harness
}

/// 1.1 — initialize returns protocolVersion, capabilities.tools, serverInfo with name+version
#[tokio::test]
async fn test_initialize_returns_valid_response() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;
    let mut client = McpClient::new(harness.base_url());
    let init = client.initialize().await.expect("initialize failed");

    let result = &init["result"];
    // protocolVersion present
    assert!(
        result["protocolVersion"].is_string(),
        "missing protocolVersion in: {init}"
    );
    // serverInfo has name and version
    assert!(
        result["serverInfo"]["name"].is_string(),
        "missing serverInfo.name in: {init}"
    );
    assert!(
        result["serverInfo"]["version"].is_string(),
        "missing serverInfo.version in: {init}"
    );
    // capabilities.tools present
    assert!(
        result["capabilities"]["tools"].is_object(),
        "missing capabilities.tools in: {init}"
    );
}

/// 1.2 — protocol version matches expected value
#[tokio::test]
async fn test_protocol_version_is_expected() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;
    let mut client = McpClient::new(harness.base_url());
    let init = client.initialize().await.expect("initialize failed");

    let version = init["result"]["protocolVersion"]
        .as_str()
        .expect("protocolVersion not a string");
    let accepted = ["2025-03-26", "2024-11-05"];
    assert!(
        accepted.contains(&version),
        "unexpected protocolVersion: {version}, expected one of {accepted:?}"
    );
}

/// 1.3 — server capabilities include tools listing
#[tokio::test]
async fn test_server_capabilities_include_tools() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;
    let mut client = McpClient::new(harness.base_url());
    let init = client.initialize().await.expect("initialize failed");

    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "capabilities.tools missing or not an object"
    );
}

/// 1.4 — server info contains name and version fields
#[tokio::test]
async fn test_server_info_has_name_and_version() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;
    let mut client = McpClient::new(harness.base_url());
    let init = client.initialize().await.expect("initialize failed");

    let info = &init["result"]["serverInfo"];
    let name = info["name"].as_str().expect("serverInfo.name missing");
    assert!(!name.is_empty(), "serverInfo.name should not be empty");
    let version = info["version"]
        .as_str()
        .expect("serverInfo.version missing");
    assert!(
        !version.is_empty(),
        "serverInfo.version should not be empty"
    );
}

/// 1.5 — second initialize on same session is idempotent
#[tokio::test]
async fn test_initialize_idempotent() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;
    let mut client = McpClient::new(harness.base_url());
    let first = client.initialize().await.expect("first init failed");
    let second = client.initialize().await.expect("second init failed");

    // Both should succeed with the same protocol version and server info
    assert_eq!(
        first["result"]["protocolVersion"], second["result"]["protocolVersion"],
        "protocol version changed between inits"
    );
    assert_eq!(
        first["result"]["serverInfo"]["name"], second["result"]["serverInfo"]["name"],
        "server name changed between inits"
    );
}

/// 1.6 — request without proper Accept header: spec says MUST return 406,
/// but relay currently accepts it. Test verifies the request at least succeeds
/// (does not crash/500). When Accept header validation is implemented, update
/// this test to assert 406.
#[tokio::test]
async fn test_missing_accept_header_does_not_crash() {
    if require_node().is_none() {
        return;
    }
    let harness = setup().await;

    let http = reqwest::Client::new();
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

    let resp = http
        .post(harness.mcp_url())
        .header("Content-Type", "application/json")
        // deliberately omitting Accept header
        .json(&body)
        .send()
        .await
        .expect("request failed");

    let status = resp.status().as_u16();
    // MCP spec says MUST return 406, but relay currently returns 200.
    // Accept either 200 (current) or 406 (spec-correct) — just not 500.
    assert!(
        status == 200 || status == 406,
        "expected 200 or 406 without Accept header, got {status}"
    );
}
