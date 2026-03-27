use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::{HealthStatus, McpAdapter};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh")
}

#[tokio::test]
async fn test_stdio_full_lifecycle() {
    let config = StdioConfig {
        command: "bash".to_string(),
        args: vec![fixture_path().to_string_lossy().to_string()],
        env: HashMap::new(),
    };

    let mut adapter = StdioAdapter::new(config);

    // Should start as Stopped
    assert_eq!(adapter.health(), HealthStatus::Stopped);

    // Initialize (spawn + handshake)
    adapter.initialize().await.expect("initialize failed");
    assert_eq!(adapter.health(), HealthStatus::Healthy);

    // List tools
    let tools = adapter.list_tools().await.expect("list_tools failed");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description.as_deref(), Some("Echoes input back"));

    // Call tool
    let result = adapter
        .call_tool("echo", json!({"message": "hello world"}))
        .await
        .expect("call_tool failed");
    let content = result.get("content").expect("missing content");
    let text = content[0].get("text").expect("missing text").as_str().unwrap();
    assert!(text.contains("hello world"));

    // Shutdown
    adapter.shutdown().await.expect("shutdown failed");
    assert_eq!(adapter.health(), HealthStatus::Stopped);
}

#[tokio::test]
async fn test_stdio_spawn_nonexistent_command() {
    let config = StdioConfig {
        command: "/nonexistent/command/that/does/not/exist".to_string(),
        args: vec![],
        env: HashMap::new(),
    };

    let mut adapter = StdioAdapter::new(config);
    let result = adapter.initialize().await;
    assert!(result.is_err());
}

