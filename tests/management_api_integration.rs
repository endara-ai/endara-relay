//! Integration test: Management API endpoints.
//!
//! Tests the management API routes: /api/status, /api/endpoints,
//! /api/endpoints/:name/tools, /api/endpoints/:name/refresh,
//! /api/endpoints/:name/logs, /api/config.

mod common;

use async_trait::async_trait;
use common::wait::{poll_until, wait_http_ready};
use endara_relay::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use endara_relay::config::{Config, EndpointConfig, ObservabilityConfig, RelayConfig, Transport};
use endara_relay::management::{management_routes, ManagementState};
use endara_relay::observability::payloads::PayloadStore;
use endara_relay::observability::pipeline::Observability;
use endara_relay::observability::store::{CallRecord, Store};
use endara_relay::registry::AdapterRegistry;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
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
            allow_insecure_oauth: None,
            toon_output: None,
            startup_init_timeout_secs: None,
            session_identity_max_sessions: None,
            validate_inputs: None,
            observability: ObservabilityConfig::default(),
            log_retention_days: None,
            write_dirs: None,
            listen_ips: None,
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
            server_type_override: None,
            isolation: Some("none".to_string()),
            container_image: None,
            mounts: None,
            auth: None,
        }],
        profiles: None,
        organizations: Vec::new(),
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
        profile_registry: None,
        event_bus: None,
    };

    let app = management_routes(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    // Wait until the server is accepting connections.
    assert!(
        wait_http_ready(&format!("http://{addr}/"), Duration::from_secs(10)).await,
        "management server did not become ready within 10s"
    );

    (addr, handle)
}

#[tokio::test]
async fn test_management_api_status() {
    let tools = vec![ToolInfo {
        name: "echo".into(),
        description: Some("Echoes input".into()),
        input_schema: json!({"type": "object"}),
        annotations: None,
        ..Default::default()
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
        ..Default::default()
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
            ..Default::default()
        },
        ToolInfo {
            name: "write_file".into(),
            description: Some("Write a file".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
            ..Default::default()
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
        ..Default::default()
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

#[tokio::test]
async fn test_management_api_network_interfaces() {
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/network-interfaces", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();

    // test_config() has listen_ips: None → echoed as an empty list.
    assert_eq!(body["listen_ips"], json!([]));

    // The interface list is machine-dependent, but the filtering invariants
    // are not: every entry has the documented shape and re-classifies as
    // eligible (never loopback, unspecified, link-local, or public).
    let interfaces = body["interfaces"].as_array().expect("interfaces array");
    for iface in interfaces {
        assert!(iface["name"].as_str().is_some());
        let ip_str = iface["ip"].as_str().expect("ip string");
        let ip: std::net::IpAddr = ip_str.parse().expect("valid IP literal");
        assert_eq!(
            endara_relay::listen_ips::classify_listen_ip(ip),
            endara_relay::listen_ips::ListenIpClass::Eligible,
            "route returned ineligible address {ip}"
        );
        assert!(!ip.is_loopback());
        assert!(!ip.is_unspecified());
        let family = iface["family"].as_str().unwrap();
        assert!(matches!(family, "v4" | "v6"));
        assert_eq!(family == "v4", ip.is_ipv4());
        assert!(matches!(
            iface["kind"].as_str().unwrap(),
            "private" | "cgnat" | "ula"
        ));
    }
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
        profile_registry: None,
        event_bus: None,
    };

    let app = management_routes(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    // Wait until the server is accepting connections.
    assert!(
        wait_http_ready(&format!("http://{addr}/"), Duration::from_secs(10)).await,
        "management server did not become ready within 10s"
    );
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
isolation = "none"
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
isolation = "none"
command = "echo"
args = ["hello"]

[[endpoints]]
name = "new-ep"
transport = "stdio"
isolation = "none"
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

    // Wait until the reloaded adapter's endpoint shows up in the API.
    assert!(
        poll_until(Duration::from_secs(10), || async {
            let Ok(resp) = client
                .get(format!("http://{}/api/endpoints", addr))
                .send()
                .await
            else {
                return false;
            };
            let Ok(endpoints) = resp.json::<Value>().await else {
                return false;
            };
            endpoints
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| e["name"].as_str())
                        .any(|n| n == "new-ep")
                })
                .unwrap_or(false)
        })
        .await,
        "config reload: new-ep never appeared in /api/endpoints within 10s"
    );

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
isolation = "none"
command = "echo"
args = ["hello"]

[[endpoints]]
name = "other-ep"
transport = "stdio"
isolation = "none"
command = "cat"
"#;
    std::fs::write(&config_file, toml_content).unwrap();

    let tools = vec![ToolInfo {
        name: "echo".into(),
        description: Some("Echoes input".into()),
        input_schema: json!({"type": "object"}),
        annotations: None,
        ..Default::default()
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
            ..Default::default()
        },
        ToolInfo {
            name: "write_file".into(),
            description: Some("Write a file".into()),
            input_schema: json!({"type": "object"}),
            annotations: Some(json!({"readOnly": true})),
            ..Default::default()
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
        ..Default::default()
    }];
    let unhealthy_tools = vec![
        ToolInfo {
            name: "ping".into(),
            description: Some("Ping server".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
            ..Default::default()
        },
        ToolInfo {
            name: "status".into(),
            description: Some("Server status".into()),
            input_schema: json!({"type": "object"}),
            annotations: None,
            ..Default::default()
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
        ..Default::default()
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

// ---------------------------------------------------------------------------
// Observability API (R5)
// ---------------------------------------------------------------------------

/// Build a seeded `CallRecord` for the observability store fixtures.
fn obs_record(server: &str, uid: &str, ts: i64, success: bool, dur: i64, tool: &str) -> CallRecord {
    CallRecord {
        request_uid: Some(uid.to_string()),
        server_name: Some(server.to_string()),
        endpoint: Some(server.to_string()),
        tool: tool.to_string(),
        ts_start: ts,
        ts_end: ts + dur,
        duration_ms: dur,
        success,
        request_bytes: 10,
        response_bytes: 20,
        ..Default::default()
    }
}

/// Start a management server with observability wired into the registry and the
/// store/payload buffer pre-seeded with three calls (two `alpha`, one `beta`).
/// Only `uid-a1` has a buffered payload. Returns the seeded handles so tests can
/// assert side effects (e.g. purge).
async fn start_observability_server(
    config_path: Option<std::path::PathBuf>,
    store_payloads: bool,
) -> (
    SocketAddr,
    Arc<Store>,
    Arc<PayloadStore>,
    tokio::task::JoinHandle<()>,
) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
    store
        .insert_batch(&[
            obs_record("alpha", "uid-a1", 1000, true, 10, "alpha__do"),
            obs_record("alpha", "uid-a2", 2000, false, 100, "alpha__do"),
            obs_record("beta", "uid-b1", 3000, true, 30, "beta__go"),
        ])
        .unwrap();
    payloads.insert("uid-a1", &json!({"a": 1}), &json!({"ok": true}), false);

    let obs_cfg = ObservabilityConfig {
        enabled: true,
        store_payloads,
        ..ObservabilityConfig::default()
    };
    let obs = Observability::new(&obs_cfg, Arc::clone(&store), Arc::clone(&payloads));
    let registry = Arc::new(AdapterRegistry::new().with_observability(obs));

    let state = ManagementState {
        registry,
        config: Arc::new(tokio::sync::RwLock::new(test_config())),
        start_time: Instant::now(),
        config_path,
        oauth_flow_manager: None,
        relay_port: 9400,
        oauth_adapter_inners: None,
        token_manager: None,
        setup_manager: None,
        profile_registry: None,
        event_bus: None,
    };

    let app = management_routes(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    // Wait until the server is accepting connections.
    assert!(
        wait_http_ready(&format!("http://{addr}/"), Duration::from_secs(10)).await,
        "observability server did not become ready within 10s"
    );
    (addr, store, payloads, handle)
}

#[tokio::test]
async fn test_observability_calls_list_filter_and_paging() {
    let (addr, _store, _payloads, _handle) = start_observability_server(None, true).await;
    let client = reqwest::Client::new();

    // Full list, newest-first. The first page returns a slim per-row DTO and
    // omits `nextCursor` because it fits under the default limit.
    let resp = client
        .get(format!("http://{}/api/observability/calls", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let calls = body["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["tsStart"], 3000);
    assert_eq!(calls[0]["serverName"], "beta");
    assert_eq!(body["limit"], 100);
    assert!(body.get("nextCursor").is_none() || body["nextCursor"].is_null());
    // The slim list DTO drops transport/client/payload-byte detail.
    assert!(calls[0].get("transport").is_none());
    assert!(calls[0].get("clientName").is_none());

    // Filter by server.
    let resp = client
        .get(format!(
            "http://{}/api/observability/calls?server_name=alpha",
            addr
        ))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["calls"].as_array().unwrap().len(), 2);

    // Filter by success.
    let resp = client
        .get(format!(
            "http://{}/api/observability/calls?success=false",
            addr
        ))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    let calls = body["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["requestUid"], "uid-a2");

    // Keyset paging: a limit-1 page returns the newest record plus a
    // `nextCursor` token; feeding it back walks to the next-newest record.
    let resp = client
        .get(format!("http://{}/api/observability/calls?limit=1", addr))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    let calls = body["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tsStart"], 3000);
    let cursor = body["nextCursor"]
        .as_str()
        .expect("nextCursor on full page");

    let resp = client
        .get(format!(
            "http://{}/api/observability/calls?limit=1&cursor={}",
            addr, cursor
        ))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    let calls = body["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tsStart"], 2000);

    // An unparseable cursor is rejected as 400 — the token is opaque, but
    // garbage in must not silently return the first page.
    let resp = client
        .get(format!(
            "http://{}/api/observability/calls?cursor=not-a-real-token",
            addr
        ))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn test_observability_call_detail_payload_states() {
    let (addr, _store, _payloads, _handle) = start_observability_server(None, true).await;
    let client = reqwest::Client::new();

    // Buffered payload present → status "stored".
    let resp = client
        .get(format!("http://{}/api/observability/calls/uid-a1", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["record"]["serverName"], "alpha");
    assert_eq!(body["payloadStatus"], "stored");
    assert_eq!(body["payloads"]["request"], "{\"a\":1}");

    // No buffered payload → status "expired", no payloads field.
    let resp = client
        .get(format!("http://{}/api/observability/calls/uid-a2", addr))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["payloadStatus"], "expired");
    assert!(body.get("payloads").is_none() || body["payloads"].is_null());

    // Unknown request_uid → 404.
    let resp = client
        .get(format!("http://{}/api/observability/calls/missing", addr))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn test_observability_call_detail_payloads_disabled() {
    let (addr, _store, _payloads, _handle) = start_observability_server(None, false).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/observability/calls/uid-a1", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["payloadStatus"], "disabled");
    assert!(body.get("payloads").is_none() || body["payloads"].is_null());
}

#[tokio::test]
async fn test_observability_aggregates() {
    let (addr, _store, _payloads, _handle) = start_observability_server(None, true).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "http://{}/api/observability/aggregates?since=0&until=10000000&bucket_seconds=60",
            addr
        ))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    let buckets = body["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty());
    // Global series (server omitted) present with all three calls.
    let global_total: u64 = buckets
        .iter()
        .filter(|b| b.get("server").is_none() || b["server"].is_null())
        .map(|b| b["count"].as_u64().unwrap())
        .sum();
    assert_eq!(global_total, 3);

    let summary = &body["summary"];
    assert_eq!(summary["enabled"], true);
    assert_eq!(summary["storePayloads"], true);
    assert_eq!(summary["dropped"], 0);
    assert_eq!(summary["payloadBufferLen"], 1);
}

#[tokio::test]
async fn test_observability_purge() {
    let (addr, store, payloads, _handle) = start_observability_server(None, true).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/api/observability/purge", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // Both tiers are now empty.
    assert_eq!(
        store
            .query(
                &endara_relay::observability::store::QueryFilter::default(),
                10,
                None,
            )
            .unwrap()
            .len(),
        0
    );
    assert!(payloads.is_empty());

    let resp = client
        .get(format!("http://{}/api/observability/calls", addr))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["calls"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_observability_config_get_put_persists() {
    let dir = std::env::temp_dir().join(format!("relay-integ-obs-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_file = dir.join("config.toml");
    std::fs::write(&config_file, "[relay]\nmachine_name = \"test-machine\"\n").unwrap();

    let (addr, _store, _payloads, _handle) =
        start_observability_server(Some(config_file.clone()), true).await;
    let client = reqwest::Client::new();

    // GET returns the in-memory defaults.
    let resp = client
        .get(format!("http://{}/api/observability/config", addr))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["enabled"], true);
    assert_eq!(body["payload_window_minutes"], 10);

    // PUT a modified config.
    let resp = client
        .put(format!("http://{}/api/observability/config", addr))
        .json(&json!({
            "enabled": false,
            "store_payloads": false,
            "payload_window_minutes": 30,
            "record_retention_days": 3,
            "max_db_size_mb": 512,
            "max_payload_bytes": 1024,
            "payload_buffer_budget_mb": 64
        }))
        .send()
        .await
        .expect("request failed");
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["enabled"], false);
    assert_eq!(body["payload_window_minutes"], 30);

    // The change is persisted to disk under [relay.observability].
    let written = std::fs::read_to_string(&config_file).unwrap();
    assert!(written.contains("[relay.observability]"));
    assert!(written.contains("enabled = false"));
    assert!(written.contains("payload_window_minutes = 30"));
    assert!(written.contains("machine_name = \"test-machine\""));

    // GET now reflects the new in-memory baseline.
    let resp = client
        .get(format!("http://{}/api/observability/config", addr))
        .send()
        .await
        .expect("request failed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["enabled"], false);
    assert_eq!(body["max_db_size_mb"], 512);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_observability_block_in_sanitized_config() {
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
    let obs = &body["relay"]["observability"];
    assert_eq!(obs["enabled"], true);
    assert_eq!(obs["store_payloads"], true);
    assert_eq!(obs["payload_window_minutes"], 10);
}

#[tokio::test]
async fn test_observability_unavailable_when_unwired() {
    // The default harness wires no observability handle → 503.
    let (addr, _handle) =
        start_management_server(vec![("echo-ep", MockAdapter::healthy_with_tools(vec![]))]).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("http://{}/api/observability/calls", addr))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status().as_u16(), 503);
}
