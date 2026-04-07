//! Integration test: Management API endpoints.
//!
//! Tests the management API routes: /api/status, /api/endpoints,
//! /api/endpoints/:name/tools, /api/endpoints/:name/refresh,
//! /api/endpoints/:name/logs, /api/config.

use async_trait::async_trait;
use endara_relay::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use endara_relay::config::{Config, EndpointConfig, RelayConfig, Transport};
use endara_relay::management::{management_routes, ManagementState};
use endara_relay::registry::AdapterRegistry;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;

/// Mock adapter for management API tests.
struct MockAdapter {
    health: HealthStatus,
    tools: Vec<ToolInfo>,
    stderr: Vec<String>,
}

impl MockAdapter {
    fn healthy_with_tools(tools: Vec<ToolInfo>) -> Self {
        Self {
            health: HealthStatus::Healthy,
            tools,
            stderr: vec![],
        }
    }

    fn unhealthy() -> Self {
        Self {
            health: HealthStatus::Unhealthy("test error".into()),
            tools: vec![],
            stderr: vec![],
        }
    }

    fn unhealthy_with_tools(tools: Vec<ToolInfo>) -> Self {
        Self {
            health: HealthStatus::Unhealthy("test error".into()),
            tools,
            stderr: vec![],
        }
    }

    fn with_stderr(mut self, lines: Vec<String>) -> Self {
        self.stderr = lines;
        self
    }
}

#[async_trait]
impl McpAdapter for MockAdapter {
    async fn initialize(&mut self) -> Result<(), AdapterError> {
        self.health = HealthStatus::Healthy;
        Ok(())
    }
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        Ok(self.tools.clone())
    }
    async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, AdapterError> {
        Ok(json!({"content": [{"type": "text", "text": "mock response"}]}))
    }
    fn health(&self) -> HealthStatus {
        self.health.clone()
    }
    async fn shutdown(&mut self) -> Result<(), AdapterError> {
        self.health = HealthStatus::Stopped;
        Ok(())
    }
    async fn stderr_lines(&self) -> Vec<String> {
        self.stderr.clone()
    }
}

fn test_config() -> Config {
    Config {
        relay: RelayConfig {
            machine_name: "test-machine".to_string(),
            local_js_execution: None,
            token_dir: None,
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

async fn start_management_server(
    adapters: Vec<(&str, MockAdapter)>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
    for (name, adapter) in adapters {
        registry
            .register(
                name.to_string(),
                Box::new(adapter),
                "stdio".to_string(),
                None,
                Some(name.to_string()),
            )
            .await;
    }
    let registry = Arc::new(registry);
    let state = ManagementState {
        registry,
        config: Arc::new(tokio::sync::RwLock::new(test_config())),
        start_time: Instant::now(),
        config_path: None,
        oauth_flow_manager: None,
        relay_port: 9400,
        oauth_adapter_inners: None,
        token_manager: None,
        setup_manager: None,
    };

    let app = management_routes(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    // Give server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (addr, handle)
}

#[tokio::test]
async fn test_management_api_status() {
    let tools = vec![ToolInfo {
        name: "echo".into(),
        description: Some("Echoes input".into()),
        input_schema: json!({"type": "object"}),
        annotations: None,
    }];
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(tools))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/status", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["endpoint_count"], 1);
    assert_eq!(body["healthy_count"], 1);
}

#[tokio::test]
async fn test_management_api_endpoints() {
    let tools = vec![ToolInfo {
        name: "t1".into(),
        description: None,
        input_schema: json!({}),
        annotations: None,
    }];
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(tools))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/endpoints", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "echo-ep");
    assert_eq!(arr[0]["health"], "healthy");
}

#[tokio::test]
async fn test_management_api_endpoint_tools() {
    let tools = vec![
        ToolInfo {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
        ToolInfo {
            name: "write_file".into(),
            description: Some("Write a file".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
    ];
    let (addr, _handle) =
        start_management_server(vec![("fs-ep", MockAdapter::healthy_with_tools(tools))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/endpoints/fs-ep/tools", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "read_file");
    assert_eq!(arr[1]["name"], "write_file");
}

#[tokio::test]
async fn test_management_api_refresh_endpoint() {
    let tools = vec![ToolInfo {
        name: "t1".into(),
        description: None,
        input_schema: json!({}),
        annotations: None,
    }];
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(tools))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/api/endpoints/echo-ep/refresh", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(body["message"].as_str().unwrap().contains("1 tools"));
}

#[tokio::test]
async fn test_management_api_endpoint_logs() {
    let mock = MockAdapter::healthy_with_tools(vec![])
        .with_stderr(vec!["log line 1".into(), "log line 2".into()]);
    let (addr, _handle) = start_management_server(vec![("echo-ep", mock)]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/endpoints/echo-ep/logs", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "log line 1");
    assert_eq!(lines[1], "log line 2");
}

#[tokio::test]
async fn test_management_api_config() {
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/config", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["relay"]["machine_name"], "test-machine");
    let endpoints = body["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0]["name"], "echo");
    assert_eq!(endpoints[0]["transport"], "stdio");
}

async fn start_management_server_with_config(
    adapters: Vec<(&str, MockAdapter)>,
    config_path: std::path::PathBuf,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new();
    for (name, adapter) in adapters {
        registry
            .register(
                name.to_string(),
                Box::new(adapter),
                "stdio".to_string(),
                None,
                Some(name.to_string()),
            )
            .await;
    }
    let registry = Arc::new(registry);
    let state = ManagementState {
        registry,
        config: Arc::new(tokio::sync::RwLock::new(test_config())),
        start_time: Instant::now(),
        config_path: Some(config_path),
        oauth_flow_manager: None,
        relay_port: 9400,
        oauth_adapter_inners: None,
        token_manager: None,
        setup_manager: None,
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
async fn test_management_api_config_reload() {
    // Write a temp config file with one endpoint
    let dir = std::env::temp_dir().join(format!("relay-integ-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_file = dir.join("config.toml");
    let initial_toml = r#"[relay]
machine_name = "test-machine"

[[endpoints]]
name = "echo-ep"
transport = "stdio"
command = "echo"
args = ["hello"]
"#;
    std::fs::write(&config_file, initial_toml).unwrap();

    let (addr, _handle) = start_management_server_with_config(
        vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))],
        config_file.clone(),
    )
    .await;
    let client = reqwest::Client::new();

    // Modify config file on disk to add a second endpoint
    let updated_toml = r#"[relay]
machine_name = "test-machine"

[[endpoints]]
name = "echo-ep"
transport = "stdio"
command = "echo"
args = ["hello"]

[[endpoints]]
name = "new-ep"
transport = "stdio"
command = "cat"
"#;
    std::fs::write(&config_file, updated_toml).unwrap();

    // POST /api/config/reload
    let resp = client
        .post(format!("http://{}/api/config/reload", addr))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["message"], "config reloaded");

    // Allow a moment for the adapter to initialize
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // GET /api/endpoints — should now include the new endpoint
    let resp = client
        .get(format!("http://{}/api/endpoints", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let endpoints: Value = resp.json().await.unwrap();
    let arr = endpoints.as_array().unwrap();
    let names: Vec<&str> = arr.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(
        names.contains(&"new-ep"),
        "Expected new-ep in endpoints, got: {:?}",
        names
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_management_api_delete_endpoint() {
    // Write a temp config file
    let dir = std::env::temp_dir().join(format!("relay-integ-delete-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_file = dir.join("config.toml");
    let toml_content = r#"[relay]
machine_name = "test-machine"

[[endpoints]]
name = "echo-ep"
transport = "stdio"
command = "echo"
args = ["hello"]

[[endpoints]]
name = "other-ep"
transport = "stdio"
command = "cat"
"#;
    std::fs::write(&config_file, toml_content).unwrap();

    let tools = vec![ToolInfo {
        name: "echo".into(),
        description: Some("Echoes input".into()),
        input_schema: json!({"type": "object"}),
        annotations: None,
    }];
    let (addr, _handle) = start_management_server_with_config(
        vec![("echo-ep", MockAdapter::healthy_with_tools(tools))],
        config_file.clone(),
    )
    .await;
    let client = reqwest::Client::new();

    // Delete the endpoint
    let resp = client
        .delete(format!("http://{}/api/endpoints/echo-ep", addr))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "removed");
    assert_eq!(body["name"], "echo-ep");

    // Verify config file no longer contains echo-ep
    let updated = std::fs::read_to_string(&config_file).unwrap();
    assert!(!updated.contains("echo-ep"));
    assert!(updated.contains("other-ep"));

    // Try deleting a non-existent endpoint
    let resp = client
        .delete(format!("http://{}/api/endpoints/nonexistent", addr))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Endpoint not found"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_management_api_catalog() {
    let tools = vec![
        ToolInfo {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
        ToolInfo {
            name: "write_file".into(),
            description: Some("Write a file".into()),
            input_schema: json!({"type": "object"}),
            annotations: Some(json!({"readOnly": true})),
        },
    ];
    let (addr, _handle) = start_management_server(vec![
        ("fs-ep", MockAdapter::healthy_with_tools(tools)),
        ("bad-ep", MockAdapter::unhealthy()),
    ])
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/catalog", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();

    // Only healthy endpoint tools appear (unhealthy has empty tools list)
    assert_eq!(arr.len(), 2);

    // Find the entries by name (order may vary due to HashMap)
    let read_entry = arr
        .iter()
        .find(|e| e["name"].as_str().unwrap().contains("read_file"))
        .expect("read_file entry not found");
    let write_entry = arr
        .iter()
        .find(|e| e["name"].as_str().unwrap().contains("write_file"))
        .expect("write_file entry not found");

    // Check prefixed names: no collision, so format is {endpoint}__{tool}
    assert!(read_entry["name"]
        .as_str()
        .unwrap()
        .contains("fs-ep__read_file"));
    assert!(write_entry["name"]
        .as_str()
        .unwrap()
        .contains("fs-ep__write_file"));

    // Check source endpoint
    assert_eq!(read_entry["endpoint"], "fs-ep");
    assert_eq!(write_entry["endpoint"], "fs-ep");

    // Check availability
    assert_eq!(read_entry["available"], true);
    assert_eq!(write_entry["available"], true);

    // Check descriptions (enriched with endpoint label by merged_catalog)
    assert_eq!(read_entry["description"], "[fs-ep] Read a file");
    assert_eq!(write_entry["description"], "[fs-ep] Write a file");

    // Check annotations (only present on write_file)
    assert!(read_entry.get("annotations").is_none() || read_entry["annotations"].is_null());
    assert_eq!(write_entry["annotations"]["readOnly"], true);

    // Check inputSchema is present
    assert_eq!(read_entry["inputSchema"]["type"], "object");
}

#[tokio::test]
async fn test_management_api_test_connection_unknown_transport() {
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/api/test-connection", addr))
        .json(&json!({ "transport": "grpc" }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Unknown transport"));
}

#[tokio::test]
async fn test_management_api_test_connection_bad_command() {
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/api/test-connection", addr))
        .json(&json!({
            "transport": "stdio",
            "command": "/nonexistent/binary/that/does/not/exist"
        }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn test_management_api_catalog_with_unhealthy_endpoints() {
    let healthy_tools = vec![ToolInfo {
        name: "read_file".into(),
        description: Some("Read a file".into()),
        input_schema: json!({"type": "object"}),
        annotations: None,
    }];
    let unhealthy_tools = vec![
        ToolInfo {
            name: "ping".into(),
            description: Some("Ping server".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
        ToolInfo {
            name: "status".into(),
            description: Some("Server status".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
        },
    ];
    let (addr, _handle) = start_management_server(vec![
        ("healthy-ep", MockAdapter::healthy_with_tools(healthy_tools)),
        (
            "sick-ep",
            MockAdapter::unhealthy_with_tools(unhealthy_tools),
        ),
    ])
    .await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/catalog", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();

    // All tools should be present: 1 healthy + 2 unhealthy = 3
    assert_eq!(
        arr.len(),
        3,
        "expected 3 catalog entries, got {}",
        arr.len()
    );

    // Find the healthy tool
    let read_entry = arr
        .iter()
        .find(|e| e["name"].as_str().unwrap().contains("read_file"))
        .expect("read_file entry not found");
    assert_eq!(read_entry["available"], true);
    assert_eq!(read_entry["endpoint"], "healthy-ep");

    // Find the unhealthy tools
    let ping_entry = arr
        .iter()
        .find(|e| e["name"].as_str().unwrap().contains("ping"))
        .expect("ping entry not found");
    assert_eq!(ping_entry["available"], false);
    assert_eq!(ping_entry["endpoint"], "sick-ep");
    assert_eq!(
        ping_entry["description"],
        "[⚠️ UNAVAILABLE] [sick-ep] Ping server"
    );

    let status_entry = arr
        .iter()
        .find(|e| e["name"].as_str().unwrap().contains("status"))
        .expect("status entry not found");
    assert_eq!(status_entry["available"], false);
    assert_eq!(status_entry["endpoint"], "sick-ep");
}

#[tokio::test]
async fn test_management_api_catalog_description_enriched() {
    // The catalog API uses merged_catalog which enriches descriptions with [endpoint] prefix
    let tools = vec![ToolInfo {
        name: "read".into(),
        description: Some("Read a file".into()),
        input_schema: json!({"type": "object"}),
        annotations: None,
    }];
    let (addr, _handle) =
        start_management_server(vec![("fs-ep", MockAdapter::healthy_with_tools(tools))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/catalog", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);

    // The catalog API enriches descriptions with [endpoint] prefix
    assert_eq!(arr[0]["description"], "[fs-ep] Read a file");
}

#[tokio::test]
async fn test_management_api_test_connection_happy_path() {
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))]).await;
    let client = reqwest::Client::new();

    let echo_script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh");

    let resp = client
        .post(format!("http://{}/api/test-connection", addr))
        .json(&json!({
            "transport": "stdio",
            "command": "bash",
            "args": [echo_script.to_string_lossy()]
        }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(
        body["tool_count"].as_u64().unwrap() > 0,
        "expected at least 1 tool, got {:?}",
        body["tool_count"]
    );
    // The echo server exposes one tool called "echo"
    let tools = body["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t.as_str() == Some("echo")));
}
