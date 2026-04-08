//! §5.9 HTTP headers, CORS, and security integration tests.
//!
//! Verifies that the relay enforces proper HTTP-level constraints:
//! CORS headers, Content-Type validation, method restrictions, and
//! graceful handling of edge cases like large bodies.

mod common;

use common::config::everything_config;
use common::harness::RelayHarness;
use common::toolchain::require_node;
use serde_json::json;
use std::time::Duration;

async fn setup_harness() -> RelayHarness {
    let config = everything_config();
    let harness = RelayHarness::start(&config).await;
    harness
        .wait_healthy("everything", Duration::from_secs(30))
        .await
        .expect("everything endpoint did not become healthy");
    harness
}

fn init_body() -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1" }
        },
        "id": 1
    })
}

/// 9.1 — Response Content-Type is application/json for JSON-RPC responses.
#[tokio::test]
async fn test_response_content_type_is_json() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let resp = harness.rpc_raw(init_body()).await;
    assert!(resp.status().is_success());
    let ct = resp
        .headers()
        .get("content-type")
        .expect("response must have Content-Type")
        .to_str()
        .unwrap();
    assert!(
        ct.contains("application/json"),
        "Expected application/json, got: {ct}"
    );
}

/// 9.2 — Request without Accept header.
#[tokio::test]
async fn test_request_without_accept_header() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(harness.mcp_url())
        .header("Content-Type", "application/json")
        .json(&init_body())
        .send()
        .await
        .expect("request should not fail");
    let status = resp.status().as_u16();
    assert!(
        status == 406 || status == 200,
        "Expected 406 or 200, got: {status}"
    );
}

/// 9.3 — GET /mcp returns 405 Method Not Allowed.
#[tokio::test]
async fn test_get_mcp_returns_405() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(harness.mcp_url())
        .send()
        .await
        .expect("GET should not fail");
    assert_eq!(resp.status().as_u16(), 405, "GET /mcp should return 405");
}

/// 9.4 — Request with Origin: http://evil.com → no ACAO header for evil origin.
#[tokio::test]
async fn test_evil_origin_blocked() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(harness.mcp_url())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "http://evil.com")
        .json(&init_body())
        .send()
        .await
        .expect("request should not fail");
    let acao = resp.headers().get("access-control-allow-origin");
    if let Some(val) = acao {
        let v = val.to_str().unwrap_or("");
        assert!(
            !v.contains("evil.com") && v != "*",
            "CORS should not allow evil.com, got: {v}"
        );
    }
}

/// 9.5 — Request with localhost Origin → allowed (CORS headers present).
#[tokio::test]
async fn test_localhost_origin_allowed() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let origin = format!("http://127.0.0.1:{}", harness.port);
    let client = reqwest::Client::new();
    let resp = client
        .post(harness.mcp_url())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", &origin)
        .json(&init_body())
        .send()
        .await
        .expect("request should not fail");
    assert!(
        resp.status().is_success(),
        "Localhost should be allowed, got: {}",
        resp.status()
    );
    assert!(
        resp.headers().get("access-control-allow-origin").is_some(),
        "Expected ACAO for localhost origin"
    );
}

/// 9.6 — OPTIONS preflight returns appropriate CORS headers.
#[tokio::test]
async fn test_cors_preflight_options() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let origin = format!("http://127.0.0.1:{}", harness.port);
    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, harness.mcp_url())
        .header("Origin", &origin)
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type")
        .send()
        .await
        .expect("OPTIONS should not fail");
    let status = resp.status().as_u16();
    assert!(
        status == 200 || status == 204,
        "OPTIONS should return 200/204, got: {status}"
    );
    let h = resp.headers();
    assert!(
        h.get("access-control-allow-origin").is_some(),
        "Preflight must include ACAO"
    );
    assert!(
        h.get("access-control-allow-methods").is_some(),
        "Preflight must include ACAM"
    );
}

/// 9.extra — POST with wrong Content-Type (text/plain) returns error.
#[tokio::test]
async fn test_wrong_content_type_rejected() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let client = reqwest::Client::new();
    let body_str = serde_json::to_string(&init_body()).unwrap();
    let resp = client
        .post(harness.mcp_url())
        .header("Content-Type", "text/plain")
        .header("Accept", "application/json, text/event-stream")
        .body(body_str)
        .send()
        .await
        .expect("request should not fail");
    let status = resp.status().as_u16();
    assert!(
        status == 415 || status == 400 || status == 422,
        "Wrong Content-Type should return 415/400/422, got: {status}"
    );
}

/// 9.extra — Very large JSON body (>1MB) is handled gracefully.
#[tokio::test]
async fn test_large_body_handled_gracefully() {
    if require_node().is_none() {
        return;
    }
    let harness = setup_harness().await;
    let big = "x".repeat(1_100_000);
    let body = json!({"jsonrpc":"2.0","method":"initialize","params":{
        "protocolVersion":"2025-03-26","capabilities":{},
        "clientInfo":{"name":big,"version":"0.1"}},"id":1});
    let client = reqwest::Client::new();
    let resp = client
        .post(harness.mcp_url())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .expect("request should not fail");
    let status = resp.status().as_u16();
    assert!(
        status == 200 || status == 413,
        "Large body should be accepted (200) or rejected (413), got: {status}"
    );
}
