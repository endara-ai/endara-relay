//! Integration test: Management API endpoints.
//!
//! Tests the management API routes: /api/status, /api/endpoints,
//! /api/endpoints/:name/tools, /api/endpoints/:name/refresh,
//! /api/endpoints/:name/logs, /api/config.

use endara_relay::adapter::{HealthStatus, McpAdapter, ToolInfo, AdapterError};
use endara_relay::config::{Config, EndpointConfig, RelayConfig, Transport};
use endara_relay::management::{ManagementState, management_routes};
use endara_relay::registry::AdapterRegistry;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;

/// Mock adapter for management API tests.
struct MockAdapter {
    health: HealthStatus,
    tools: Vec<ToolInfo>,
}

impl MockAdapter {
    fn healthy_with_tools(tools: Vec<ToolInfo>) -> Self {
        Self {
            health: HealthStatus::Healthy,
            tools,
        }
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
}

fn test_config() -> Config {
    Config {
        relay: RelayConfig {
            machine_name: "test-machine".to_string(),
            local_js_execution: None,
        },
        endpoints: vec![EndpointConfig {
            name: "echo".to_string(),
            transport: Transport::Stdio,
            command: Some("echo".to_string()),
            args: Some(vec!["hello".to_string()]),
            url: None,
            env: None,
        }],
    }
}

async fn start_management_server(
    adapters: Vec<(&str, MockAdapter)>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let registry = AdapterRegistry::new("test-machine".into());
    for (name, adapter) in adapters {
        registry
            .register(name.to_string(), Box::new(adapter), "stdio".to_string())
            .await;
        // Add test stderr lines
        let entries = registry.entries().read().await;
        if let Some(entry) = entries.get(name) {
            let mut lines = entry.stderr_lines.write().await;
            lines.push("log line 1".to_string());
            lines.push("log line 2".to_string());
        }
    }
    let registry = Arc::new(registry);
    let state = ManagementState {
        registry,
        config: Arc::new(tokio::sync::RwLock::new(test_config())),
        start_time: Instant::now(),
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
        },
        ToolInfo {
            name: "write_file".into(),
            description: Some("Write a file".into()),
            input_schema: json!({"type": "object"}),
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
    let (addr, _handle) = start_management_server(vec![(
        "echo-ep",
        MockAdapter::healthy_with_tools(vec![]),
    )])
    .await;
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
    let (addr, _handle) = start_management_server(vec![(
        "echo-ep",
        MockAdapter::healthy_with_tools(vec![]),
    )])
    .await;
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

