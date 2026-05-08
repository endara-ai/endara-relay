//! Integration test: POST /api/oauth/setup advanced fields.
//!
//! Exercises the path where the caller provides explicit `client_id` /
//! `client_secret` in the setup request, which should bypass DCR entirely.

mod common;

use endara_relay::config::{Config, EndpointConfig, RelayConfig, Transport};
use endara_relay::management::{management_routes, ManagementState};
use endara_relay::oauth::{OAuthFlowManager, OAuthSetupManager};
use endara_relay::registry::AdapterRegistry;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use common::mock_oauth_server::MockOAuthServer;

fn empty_config() -> Config {
    Config {
        relay: RelayConfig {
            machine_name: "test-machine".to_string(),
            local_js_execution: None,
            token_dir: None,
            allow_insecure_oauth: Some(true),
        },
        endpoints: vec![EndpointConfig {
            name: "echo".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Stdio,
            command: Some("echo".to_string()),
            args: Some(vec!["hello".to_string()]),
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
            token_endpoint: None,
        }],
    }
}

async fn start_setup_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = Arc::new(AdapterRegistry::new());
    let state = ManagementState {
        registry,
        config: Arc::new(RwLock::new(empty_config())),
        start_time: Instant::now(),
        config_path: None,
        oauth_flow_manager: Some(Arc::new(OAuthFlowManager::new())),
        relay_port: 9400,
        oauth_adapter_inners: None,
        token_manager: None,
        setup_manager: Some(Arc::new(OAuthSetupManager::new())),
    };

    let app = management_routes(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, handle)
}

#[tokio::test]
async fn oauth_setup_with_client_id_skips_dcr() {
    let mock = MockOAuthServer::start().await;
    let (addr, _handle) = start_setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/api/oauth/setup", addr))
        .json(&json!({
            "name": "manual-creds",
            "url": mock.base_url(),
            "client_id": "user-supplied-client",
            "client_secret": "user-supplied-secret",
            "scopes": ["read", "write"],
        }))
        .send()
        .await
        .expect("setup request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["status"], "awaiting_auth");
    assert!(body["session_id"].as_str().is_some());
    let authorize_url = body["authorize_url"]
        .as_str()
        .expect("authorize_url present");
    assert!(authorize_url.contains("client_id=user-supplied-client"));
    assert!(authorize_url.contains("response_type=code"));
    assert!(authorize_url.contains("code_challenge_method=S256"));
    // Scopes were forwarded (form-urlencoded: spaces become '+')
    assert!(authorize_url.contains("scope=read+write"));
    assert_eq!(body["dcr_error"], Value::Null);
    assert_eq!(body["discovery"]["dcr_used"], false);

    // DCR endpoint must not have been called when client_id is supplied.
    let register_calls = mock.requests_to("/register").await;
    assert!(
        register_calls.is_empty(),
        "expected DCR /register to be skipped, got {} calls",
        register_calls.len()
    );
}

#[tokio::test]
async fn oauth_setup_without_client_id_still_uses_dcr() {
    let mock = MockOAuthServer::start().await;
    let (addr, _handle) = start_setup_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/api/oauth/setup", addr))
        .json(&json!({
            "name": "auto-dcr",
            "url": mock.base_url(),
        }))
        .send()
        .await
        .expect("setup request failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "awaiting_auth");
    assert_eq!(body["discovery"]["dcr_used"], true);

    // DCR /register should have been called exactly once
    let register_calls = mock.requests_to("/register").await;
    assert_eq!(register_calls.len(), 1);
}
