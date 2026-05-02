//! Integration test for the `/healthz` liveness endpoint.
//!
//! Boots the relay router on an ephemeral port and verifies that
//! `GET /healthz` returns a JSON body with `status`, `version`, and
//! `uptime_secs` fields, and that the `Content-Type` is JSON.

use endara_relay::js_sandbox::MetaToolHandler;
use endara_relay::registry::AdapterRegistry;
use endara_relay::server::{build_router, start_server, AppState};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

async fn setup_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
    let registry_arc = Arc::new(registry.clone());
    let state = AppState {
        registry: registry.clone(),
        js_execution_mode: Arc::new(AtomicBool::new(false)),
        meta_tool_handler: Arc::new(MetaToolHandler::new(registry_arc, Duration::from_secs(30))),
        oauth_flow_manager: None,
        token_manager: None,
        oauth_adapter_inners: None,
        setup_manager: None,
        started_at: std::time::Instant::now(),
    };
    let router = build_router(state);
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let (bound_addr, handle) = start_server(router, addr)
        .await
        .expect("server start failed");
    (bound_addr, handle)
}

#[tokio::test]
async fn healthz_returns_json_with_status_version_and_uptime() {
    let (addr, _handle) = setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/healthz", addr))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200, "expected 200 OK");

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("application/json"),
        "expected application/json Content-Type, got: {content_type}"
    );

    let body: serde_json::Value = resp.json().await.expect("response was not JSON");

    assert_eq!(body["status"], "ok", "status must be \"ok\": {body}");

    let version = body["version"].as_str().expect("version must be a string");
    assert!(!version.is_empty(), "version must be non-empty");

    let uptime = body["uptime_secs"]
        .as_u64()
        .expect("uptime_secs must be an integer >= 0");
    // u64 is always >= 0; assert it parses and is sane (< 1 day for a fresh process).
    assert!(uptime < 86_400, "uptime_secs unexpectedly large: {uptime}");
}
