use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::adapter::HealthStatus;
use crate::config::Config;
use crate::registry::AdapterRegistry;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Shared state for the management API.
#[derive(Clone)]
pub struct ManagementState {
    pub registry: Arc<AdapterRegistry>,
    pub config: Arc<RwLock<Config>>,
    pub start_time: Instant,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct StatusResponse {
    pub status: String,
    pub uptime_seconds: u64,
    pub endpoint_count: usize,
    pub healthy_count: usize,
}

#[derive(Serialize)]
pub struct EndpointInfo {
    pub name: String,
    pub transport: String,
    pub health: String,
    pub tool_count: usize,
    pub last_activity: Option<u64>,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub lines: Vec<String>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub ok: bool,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn error_response(status: StatusCode, error: &str, detail: Option<&str>) -> impl IntoResponse {
    (
        status,
        Json(ErrorResponse {
            error: error.to_string(),
            detail: detail.map(|s| s.to_string()),
        }),
    )
}

fn endpoint_not_found(name: &str) -> impl IntoResponse {
    error_response(
        StatusCode::NOT_FOUND,
        "endpoint not found",
        Some(&format!("No endpoint named '{}'. Use GET /api/endpoints to list available endpoints.", name)),
    )
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// GET /api/status
async fn get_status(State(state): State<ManagementState>) -> Json<StatusResponse> {
    let entries = state.registry.entries().read().await;
    let healthy_count = entries
        .values()
        .filter(|e| matches!(e.adapter.health(), HealthStatus::Healthy))
        .count();

    Json(StatusResponse {
        status: "ok".to_string(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        endpoint_count: entries.len(),
        healthy_count,
    })
}

/// GET /api/endpoints
async fn get_endpoints(State(state): State<ManagementState>) -> Json<Vec<EndpointInfo>> {
    let entries = state.registry.entries().read().await;
    let now = Instant::now();
    let mut endpoints: Vec<EndpointInfo> = Vec::new();
    for (name, entry) in entries.iter() {
        let tool_count = if matches!(entry.adapter.health(), HealthStatus::Healthy) {
            entry.adapter.list_tools().await.map(|t| t.len()).unwrap_or(0)
        } else {
            0
        };
        endpoints.push(EndpointInfo {
            name: name.clone(),
            transport: entry.transport.clone(),
            health: entry.adapter.health().to_string(),
            tool_count,
            last_activity: entry.last_activity.map(|t| now.duration_since(t).as_secs()),
        });
    }
    endpoints.sort_by(|a, b| a.name.cmp(&b.name));
    Json(endpoints)
}

/// POST /api/endpoints/:name/restart
async fn restart_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let mut entries = state.registry.entries().write().await;
    let Some(entry) = entries.get_mut(&name) else {
        return endpoint_not_found(&name).into_response();
    };
    if let Err(e) = entry.adapter.shutdown().await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to shutdown adapter",
            Some(&e.to_string()),
        )
        .into_response();
    }
    match entry.adapter.initialize().await {
        Ok(()) => Json(ActionResponse {
            ok: true,
            message: format!("Endpoint '{}' restarted", name),
        })
        .into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to restart adapter",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

/// POST /api/endpoints/:name/refresh
async fn refresh_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let entries = state.registry.entries().read().await;
    let Some(entry) = entries.get(&name) else {
        return endpoint_not_found(&name).into_response();
    };
    match entry.adapter.list_tools().await {
        Ok(tools) => Json(ActionResponse {
            ok: true,
            message: format!("Refreshed {} tools for '{}'", tools.len(), name),
        })
        .into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to refresh tools",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

/// GET /api/endpoints/:name/logs
async fn get_endpoint_logs(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let entries = state.registry.entries().read().await;
    let Some(entry) = entries.get(&name) else {
        return endpoint_not_found(&name).into_response();
    };
    let lines = entry.stderr_lines.read().await.clone();
    Json(LogsResponse { lines }).into_response()
}

/// GET /api/config
async fn get_config(State(state): State<ManagementState>) -> impl IntoResponse {
    let config = state.config.read().await;
    // Build a sanitized view — redact env values that came from env vars
    let sanitized: SanitizedConfig = sanitize_config(&config);
    Json(sanitized).into_response()
}

#[derive(Serialize)]
struct SanitizedConfig {
    relay: SanitizedRelay,
    endpoints: Vec<SanitizedEndpoint>,
}

#[derive(Serialize)]
struct SanitizedRelay {
    machine_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_js_execution: Option<bool>,
}

#[derive(Serialize)]
struct SanitizedEndpoint {
    name: String,
    transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<HashMap<String, String>>,
}

fn sanitize_config(config: &Config) -> SanitizedConfig {
    SanitizedConfig {
        relay: SanitizedRelay {
            machine_name: config.relay.machine_name.clone(),
            local_js_execution: config.relay.local_js_execution,
        },
        endpoints: config
            .endpoints
            .iter()
            .map(|ep| SanitizedEndpoint {
                name: ep.name.clone(),
                transport: ep.transport.to_string(),
                command: ep.command.clone(),
                args: ep.args.clone(),
                url: ep.url.clone(),
                env: ep.env.as_ref().map(|env_map| {
                    env_map
                        .keys()
                        .map(|k| (k.clone(), "***".to_string()))
                        .collect()
                }),
            })
            .collect(),
    }
}

/// POST /api/config/reload
async fn reload_config() -> Json<ActionResponse> {
    Json(ActionResponse {
        ok: true,
        message: "reload scheduled".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the management API router with all /api routes.
pub fn management_routes(state: ManagementState) -> Router {
    Router::new()
        .route("/api/status", get(get_status))
        .route("/api/endpoints", get(get_endpoints))
        .route("/api/endpoints/{name}/tools", get(get_endpoint_tools))
        .route("/api/endpoints/{name}/restart", post(restart_endpoint))
        .route("/api/endpoints/{name}/refresh", post(refresh_endpoint))
        .route("/api/endpoints/{name}/logs", get(get_endpoint_logs))
        .route("/api/config", get(get_config))
        .route("/api/config/reload", post(reload_config))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// GET /api/endpoints/:name/tools
async fn get_endpoint_tools(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let entries = state.registry.entries().read().await;
    let Some(entry) = entries.get(&name) else {
        return endpoint_not_found(&name).into_response();
    };
    match entry.adapter.list_tools().await {
        Ok(tools) => Json(tools).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to list tools",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use crate::config::{Config, EndpointConfig, RelayConfig, Transport};
    use serde_json::Value;
    use tower::ServiceExt; // for oneshot

    /// Mock adapter for testing.
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

        fn unhealthy(reason: &str) -> Self {
            Self {
                health: HealthStatus::Unhealthy(reason.to_string()),
                tools: vec![],
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
            Ok(Value::Null)
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
                env: Some(HashMap::from([("SECRET".to_string(), "s3cret".to_string())])),
            }],
        }
    }

    async fn test_state(adapters: Vec<(&str, MockAdapter)>) -> ManagementState {
        let registry = AdapterRegistry::new("test-machine".into());
        for (name, adapter) in adapters {
            registry
                .register(name.to_string(), Box::new(adapter), "stdio".to_string())
                .await;
            // Add test stderr lines
            let entries = registry.entries().read().await;
            if let Some(entry) = entries.get(name) {
                let mut lines = entry.stderr_lines.write().await;
                lines.push("line1".to_string());
                lines.push("line2".to_string());
            }
        }
        ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(test_config())),
            start_time: Instant::now(),
        }
    }

    async fn body_json(resp: axum::http::Response<Body>) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn management_status_ok() {
        let tools = vec![ToolInfo {
            name: "t1".into(),
            description: None,
            input_schema: serde_json::json!({}),
        }];
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "ok");
        assert_eq!(body["endpoint_count"], 1);
        assert_eq!(body["healthy_count"], 1);
    }

    #[tokio::test]
    async fn management_endpoints_list() {
        let tools = vec![
            ToolInfo {
                name: "t1".into(),
                description: None,
                input_schema: serde_json::json!({}),
            },
            ToolInfo {
                name: "t2".into(),
                description: None,
                input_schema: serde_json::json!({}),
            },
        ];
        let state = test_state(vec![
            ("a", MockAdapter::healthy_with_tools(tools)),
            ("b", MockAdapter::unhealthy("down")),
        ])
        .await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // sorted by name
        assert_eq!(arr[0]["name"], "a");
        assert_eq!(arr[0]["health"], "healthy");
        assert_eq!(arr[0]["tool_count"], 2);
        assert_eq!(arr[1]["name"], "b");
        assert!(arr[1]["health"].as_str().unwrap().contains("unhealthy"));
        assert_eq!(arr[1]["tool_count"], 0);
    }

    #[tokio::test]
    async fn management_endpoint_tools() {
        let tools = vec![ToolInfo {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let state = test_state(vec![("fs", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/fs/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "read_file");
    }

    #[tokio::test]
    async fn management_endpoint_not_found() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/nonexistent/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "endpoint not found");
        assert!(body["detail"]
            .as_str()
            .unwrap()
            .contains("nonexistent"));
    }

    #[tokio::test]
    async fn management_endpoint_logs() {
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(vec![]))]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/echo/logs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let lines = body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "line1");
    }

    #[tokio::test]
    async fn management_config_sanitized() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["relay"]["machine_name"], "test-machine");
        let ep = &body["endpoints"][0];
        assert_eq!(ep["name"], "echo");
        // env values should be redacted
        assert_eq!(ep["env"]["SECRET"], "***");
    }

    #[tokio::test]
    async fn management_config_reload() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/config/reload")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["message"], "reload scheduled");
    }

    #[tokio::test]
    async fn management_restart_endpoint() {
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(vec![]))]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("restarted"));
    }

    #[tokio::test]
    async fn management_refresh_endpoint() {
        let tools = vec![ToolInfo {
            name: "t1".into(),
            description: None,
            input_schema: serde_json::json!({}),
        }];
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("1 tools"));
    }

    #[tokio::test]
    async fn management_restart_not_found() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/missing/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}