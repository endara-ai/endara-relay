//! Integration test: HTTP adapter lifecycle.
//!
//! Starts fixture-http-server on a random port, creates an HttpAdapter,
//! performs initialize/list_tools/call_tool, then shuts down the server
//! and verifies the adapter detects unhealthy state.

use endara_relay::adapter::http::{HttpAdapter, HttpConfig};
use endara_relay::adapter::{HealthStatus, McpAdapter};
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

fn http_server_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-http-server").to_string()
}

/// Start the HTTP fixture server on a random port and return (child, port).
async fn start_http_server() -> (tokio::process::Child, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let child = tokio::process::Command::new(http_server_bin())
        .args(["--port", &port.to_string()])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start fixture-http-server");

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_millis(500)).await;

    (child, port)
}

#[tokio::test]
async fn test_http_adapter_full_lifecycle() {
    let (mut child, port) = start_http_server().await;
    let url = format!("http://127.0.0.1:{}/mcp", port);

    let mut adapter = HttpAdapter::new(HttpConfig::new(&url));
    assert_eq!(adapter.health(), HealthStatus::Stopped);

    // Initialize
    let init_result = timeout(Duration::from_secs(10), adapter.initialize()).await;
    assert!(init_result.is_ok(), "initialize timed out");
    init_result.unwrap().expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // List tools
    let tools = adapter.list_tools().await.expect("list_tools failed");
    assert_eq!(tools.len(), 2, "expected 2 tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"), "missing echo tool");
    assert!(names.contains(&"reverse"), "missing reverse tool");

    // Call echo tool
    let result = adapter
        .call_tool("echo", json!({"message": "http test"}))
        .await
        .expect("call_tool echo failed");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("http test"), "unexpected echo response: {}", text);

    // Call reverse tool
    let result = adapter
        .call_tool("reverse", json!({"message": "hello"}))
        .await
        .expect("call_tool reverse failed");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("olleh"), "unexpected reverse response: {}", text);

    // Shutdown adapter
    adapter.shutdown().await.expect("shutdown failed");
    assert_eq!(adapter.health(), HealthStatus::Stopped);

    // Kill the fixture server
    child.kill().await.ok();
}

#[tokio::test]
async fn test_http_adapter_server_death_detection() {
    let (mut child, port) = start_http_server().await;
    let url = format!("http://127.0.0.1:{}/mcp", port);

    let mut adapter = HttpAdapter::new(HttpConfig::new(&url).with_timeout(3));

    let init_result = timeout(Duration::from_secs(10), adapter.initialize()).await;
    assert!(init_result.is_ok(), "initialize timed out");
    init_result.unwrap().expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // Kill the server
    child.kill().await.ok();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // After server is killed, a tool call should fail
    let result = adapter.call_tool("echo", json!({"message": "after death"})).await;
    assert!(result.is_err(), "expected error after server death");

    let _ = adapter.shutdown().await;
}

