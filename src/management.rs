use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;

use tracing::warn;

use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::oauth::OAuthState;
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::HealthStatus;
use crate::config::Config;
use crate::oauth::{OAuthFlowManager, PkceChallenge};
use crate::registry::AdapterRegistry;
use crate::OAuthAdapterInners;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Shared state for the management API.
#[derive(Clone)]
pub struct ManagementState {
    pub registry: Arc<AdapterRegistry>,
    pub config: Arc<RwLock<Config>>,
    pub start_time: Instant,
    /// Path to the TOML config file on disk (used by DELETE endpoint).
    pub config_path: Option<PathBuf>,
    /// OAuth flow manager (shared across management routes).
    pub oauth_flow_manager: Option<Arc<OAuthFlowManager>>,
    /// Port the relay is listening on (used to construct redirect_uri).
    pub relay_port: u16,
    /// Per-endpoint shared OAuth adapter inner states.
    pub oauth_adapter_inners: Option<OAuthAdapterInners>,
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
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_prefix: Option<String>,
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

/// Request body for POST /api/test-connection.
#[derive(Deserialize)]
pub struct TestConnectionRequest {
    pub transport: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
}

/// Response body for POST /api/test-connection.
#[derive(Serialize)]
pub struct TestConnectionResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    pub endpoint: String,
    pub available: bool,
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
        Some(&format!(
            "No endpoint named '{}'. Use GET /api/endpoints to list available endpoints.",
            name
        )),
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
        let (health, tool_count, error) = if entry.disabled {
            ("stopped".to_string(), 0, None)
        } else if matches!(entry.adapter.health(), HealthStatus::Healthy) {
            let count = entry
                .adapter
                .list_tools()
                .await
                .map(|t| t.len())
                .unwrap_or(0);
            (entry.adapter.health().to_string(), count, None)
        } else if let HealthStatus::Unhealthy(reason) = entry.adapter.health() {
            ("offline".to_string(), 0, Some(reason))
        } else {
            (entry.adapter.health().to_string(), 0, None)
        };
        endpoints.push(EndpointInfo {
            name: name.clone(),
            transport: entry.transport.clone(),
            health,
            tool_count,
            last_activity: entry.last_activity.map(|t| now.duration_since(t).as_secs()),
            disabled: entry.disabled,
            error,
            tool_prefix: entry.tool_prefix.clone(),
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
    let mut lines = entry.adapter.stderr_lines().await;
    let activity = entry.adapter.activity_log().await;
    lines.extend(activity);
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

/// DELETE /api/endpoints/:name
///
/// Removes the named endpoint from the config file on disk. The config file
/// watcher (hot-reload) will automatically pick up the change and unload the
/// endpoint from the running registry.
async fn delete_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(config_path) = &state.config_path else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_path not configured",
            Some("The management API was not initialised with a config file path."),
        )
        .into_response();
    };

    let resolved = crate::config::expand_tilde(config_path);

    // Read the raw TOML from disk so we can do a targeted edit.
    let contents = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read config file",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };

    let mut parsed: toml::Table = match contents.parse() {
        Ok(t) => t,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to parse config file",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };

    // Remove the matching endpoint from the [[endpoints]] array.
    let found = if let Some(toml::Value::Array(endpoints)) = parsed.get_mut("endpoints") {
        let original_len = endpoints.len();
        endpoints.retain(|ep| {
            ep.get("name")
                .and_then(|v| v.as_str())
                .map(|n| n != name)
                .unwrap_or(true)
        });
        endpoints.len() < original_len
    } else {
        false
    };

    if !found {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Endpoint not found: {}", name),
                detail: None,
            }),
        )
            .into_response();
    }

    // Serialize and write back.
    let new_contents = match toml::to_string_pretty(&parsed) {
        Ok(s) => s,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to serialize config",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };

    if let Err(e) = std::fs::write(&resolved, &new_contents) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write config file",
            Some(&e.to_string()),
        )
        .into_response();
    }

    // Return success — the config watcher will pick up the file change and
    // unload the endpoint from the registry automatically.
    Json(serde_json::json!({
        "status": "removed",
        "name": name,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Persist disabled state
// ---------------------------------------------------------------------------

/// Read disabled/disabled_tools from the registry and write them back to config.toml.
async fn persist_disabled_state(state: &ManagementState) {
    let Some(ref config_path) = state.config_path else {
        return;
    };
    let mut config = state.config.write().await;

    // Read current disabled state from registry
    let entries = state.registry.entries().read().await;
    for ep_config in &mut config.endpoints {
        if let Some(entry) = entries.get(&ep_config.name) {
            ep_config.disabled = entry.disabled;
            ep_config.disabled_tools = entry.disabled_tools.iter().cloned().collect();
        }
    }
    drop(entries);

    // Write back to file
    let resolved = crate::config::expand_tilde(config_path);
    if let Ok(toml_str) = toml::to_string_pretty(&*config) {
        if let Err(e) = std::fs::write(&resolved, &toml_str) {
            warn!(error = %e, "Failed to persist disabled state");
        }
    }
}

/// POST /api/test-connection
///
/// Creates a temporary adapter from the provided config, initializes it,
/// lists tools, then shuts it down. Returns success with tool info or an error.
async fn test_connection(Json(req): Json<TestConnectionRequest>) -> impl IntoResponse {
    let transport = req.transport.to_lowercase();

    // Create a temporary adapter based on transport type
    let mut adapter: Box<dyn crate::adapter::McpAdapter> = match transport.as_str() {
        "stdio" => {
            let config = StdioConfig {
                command: req.command.unwrap_or_default(),
                args: req.args.unwrap_or_default(),
                env: req.env.unwrap_or_default(),
            };
            Box::new(StdioAdapter::new(config))
        }
        "sse" => {
            let url = req.url.unwrap_or_default();
            let mut config = SseConfig::new(url);
            config.headers = req.headers.unwrap_or_default();
            Box::new(SseAdapter::new(config))
        }
        "http" => {
            let url = req.url.unwrap_or_default();
            let mut config = HttpConfig::new(url);
            config.headers = req.headers.unwrap_or_default();
            Box::new(HttpAdapter::new(config))
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(TestConnectionResponse {
                    success: false,
                    tool_count: None,
                    tools: None,
                    error: Some(format!("Unknown transport: {}", req.transport)),
                }),
            )
                .into_response();
        }
    };

    // Initialize (handshake) with a timeout
    let init_result =
        tokio::time::timeout(std::time::Duration::from_secs(30), adapter.initialize()).await;

    match init_result {
        Ok(Ok(())) => {
            // List tools
            let tools_result = adapter.list_tools().await;
            // Always shut down regardless of list_tools result
            let _ = adapter.shutdown().await;

            match tools_result {
                Ok(tools) => Json(TestConnectionResponse {
                    success: true,
                    tool_count: Some(tools.len()),
                    tools: Some(tools.into_iter().map(|t| t.name).collect()),
                    error: None,
                })
                .into_response(),
                Err(e) => (
                    StatusCode::OK,
                    Json(TestConnectionResponse {
                        success: false,
                        tool_count: None,
                        tools: None,
                        error: Some(format!("Connected but failed to list tools: {}", e)),
                    }),
                )
                    .into_response(),
            }
        }
        Ok(Err(e)) => {
            let _ = adapter.shutdown().await;
            (
                StatusCode::OK,
                Json(TestConnectionResponse {
                    success: false,
                    tool_count: None,
                    tools: None,
                    error: Some(format!("Connection failed: {}", e)),
                }),
            )
                .into_response()
        }
        Err(_) => {
            let _ = adapter.shutdown().await;
            (
                StatusCode::OK,
                Json(TestConnectionResponse {
                    success: false,
                    tool_count: None,
                    tools: None,
                    error: Some("Connection timed out after 30 seconds".to_string()),
                }),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// OAuth route handlers
// ---------------------------------------------------------------------------

/// Response for POST /api/endpoints/:name/oauth/start
#[derive(Serialize)]
struct OAuthStartResponse {
    authorize_url: String,
}

/// Response for GET /api/endpoints/:name/oauth/status (simple)
#[derive(Serialize)]
struct OAuthStatusResponse {
    status: String,
}

/// Enhanced response for GET /api/endpoints/:name/oauth/status
#[derive(Serialize)]
struct OAuthStatusDetailedResponse {
    status: String,
    has_access_token: bool,
    has_refresh_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_refreshed_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_refresh_at: Option<u64>,
    state: String,
}

/// Response for POST /api/endpoints/:name/oauth/revoke
#[derive(Serialize)]
struct OAuthRevokeResponse {
    status: String,
    endpoint: String,
}

/// Response for POST /api/endpoints/:name/oauth/refresh
#[derive(Serialize)]
struct OAuthRefreshResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refreshed_at: Option<u64>,
}

/// POST /api/endpoints/:name/oauth/start
///
/// Generates a PKCE challenge, registers a pending flow, and returns the
/// authorization URL that the user should open in a browser.
async fn oauth_start(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Look up endpoint config
    let config = state.config.read().await;
    let ep = config.endpoints.iter().find(|e| e.name == name);
    let Some(ep) = ep else {
        return endpoint_not_found(&name).into_response();
    };

    let oauth_server_url = match &ep.oauth_server_url {
        Some(url) => url.clone(),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "endpoint is not configured for OAuth",
                Some("Missing oauth_server_url in endpoint config"),
            )
            .into_response();
        }
    };
    let client_id = match &ep.client_id {
        Some(id) => id.clone(),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "endpoint is not configured for OAuth",
                Some("Missing client_id in endpoint config"),
            )
            .into_response();
        }
    };
    let client_secret = ep.client_secret.clone();
    let scopes = ep.scopes.clone();
    drop(config);

    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            Some("OAuth flow manager not initialized"),
        )
        .into_response();
    };

    let pkce = PkceChallenge::generate();
    let code_challenge = pkce.code_challenge.clone();

    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);

    let state_param = flow_mgr
        .start_flow(
            &name,
            &oauth_server_url,
            &client_id,
            client_secret.as_deref(),
            pkce,
            &redirect_uri,
        )
        .await;

    // Build the authorization URL
    let mut authorize_url = format!(
        "{}/authorize?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        oauth_server_url.trim_end_matches('/'),
        urlencoding(&client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state_param),
        urlencoding(&code_challenge),
    );

    if let Some(ref scopes) = scopes {
        let scope_str = scopes.join(" ");
        authorize_url.push_str(&format!("&scope={}", urlencoding(&scope_str)));
    }

    Json(OAuthStartResponse { authorize_url }).into_response()
}

/// Simple URL encoding helper (percent-encode special chars).
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// GET /api/endpoints/:name/oauth/status
///
/// Returns detailed OAuth status for the endpoint including token info.
async fn oauth_status(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Verify endpoint exists in registry
    {
        let entries = state.registry.entries().read().await;
        if !entries.contains_key(&name) {
            return endpoint_not_found(&name).into_response();
        }
    }

    // Try to get detailed OAuth info from the adapter inners
    if let Some(ref inners) = state.oauth_adapter_inners {
        let inners_guard = inners.read().await;
        if let Some(inner) = inners_guard.get(&name) {
            let oauth_state = inner.state.read().await.clone();
            let tokens = inner.tokens.read().await;

            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let has_access_token = tokens.is_some();
            let has_refresh_token = tokens
                .as_ref()
                .and_then(|t| t.refresh_token.as_ref())
                .is_some();
            let expires_at = tokens.as_ref().and_then(|t| t.expires_at);
            let expires_in_seconds = expires_at.map(|exp| exp as i64 - now_secs as i64);
            let last_refreshed_at = tokens.as_ref().and_then(|t| t.issued_at);

            // Compute next_refresh_at from token expiry using the 75% rule
            let next_refresh_at = tokens
                .as_ref()
                .and_then(|t| match (t.issued_at, t.expires_at) {
                    (Some(issued), Some(expires)) if expires > issued => {
                        let lifetime = expires - issued;
                        let seventy_five_pct = issued + (lifetime * 3 / 4);
                        let five_min_before = expires.saturating_sub(300);
                        Some(std::cmp::min(seventy_five_pct, five_min_before))
                    }
                    _ => None,
                });

            let status = match oauth_state {
                OAuthState::Authenticated => "authenticated",
                OAuthState::NeedsLogin => "needs_login",
                OAuthState::Refreshing => "refreshing",
                OAuthState::AuthRequired => "auth_required",
                OAuthState::Disconnected => "disconnected",
                OAuthState::ConnectionFailed => "connection_failed",
            };

            let state_str = format!("{:?}", oauth_state);

            return Json(OAuthStatusDetailedResponse {
                status: status.to_string(),
                has_access_token,
                has_refresh_token,
                expires_at,
                expires_in_seconds,
                last_refreshed_at,
                next_refresh_at,
                state: state_str,
            })
            .into_response();
        }
    }

    // Fallback: endpoint exists but is not an OAuth endpoint
    let entries = state.registry.entries().read().await;
    let entry = entries.get(&name).unwrap();
    let status = match entry.adapter.health() {
        HealthStatus::Healthy => "authorized",
        HealthStatus::Unhealthy(ref msg) if msg == "needs login" => "needs_login",
        _ => "unhealthy",
    };

    Json(OAuthStatusResponse {
        status: status.to_string(),
    })
    .into_response()
}

/// POST /api/endpoints/:name/oauth/revoke
///
/// Disconnects the OAuth adapter and deletes tokens.
async fn oauth_revoke(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Verify endpoint exists in registry
    {
        let entries = state.registry.entries().read().await;
        if !entries.contains_key(&name) {
            return endpoint_not_found(&name).into_response();
        }
    }

    let Some(ref inners) = state.oauth_adapter_inners else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "endpoint is not configured for OAuth",
            Some("No OAuth adapter inners available"),
        )
        .into_response();
    };

    let inners_guard = inners.read().await;
    let Some(inner) = inners_guard.get(&name) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "endpoint is not configured for OAuth",
            Some(&format!("Endpoint '{}' is not an OAuth endpoint", name)),
        )
        .into_response();
    };

    let inner = inner.clone();
    drop(inners_guard);

    // Call disconnect (aborts refresh task, clears tokens, deletes from disk)
    inner.disconnect().await;

    Json(OAuthRevokeResponse {
        status: "disconnected".to_string(),
        endpoint: name,
    })
    .into_response()
}

/// POST /api/endpoints/:name/oauth/refresh
///
/// Triggers a manual token refresh.
async fn oauth_refresh(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Verify endpoint exists in registry
    {
        let entries = state.registry.entries().read().await;
        if !entries.contains_key(&name) {
            return endpoint_not_found(&name).into_response();
        }
    }

    let Some(ref inners) = state.oauth_adapter_inners else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "endpoint is not configured for OAuth",
            Some("No OAuth adapter inners available"),
        )
        .into_response();
    };

    let inners_guard = inners.read().await;
    let Some(inner) = inners_guard.get(&name) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "endpoint is not configured for OAuth",
            Some(&format!("Endpoint '{}' is not an OAuth endpoint", name)),
        )
        .into_response();
    };

    // Check current state — refuse if NeedsLogin or Disconnected
    let current_state = inner.state.read().await.clone();
    if matches!(
        current_state,
        OAuthState::NeedsLogin | OAuthState::Disconnected
    ) {
        let reason = match current_state {
            OAuthState::NeedsLogin => "endpoint has never been authenticated",
            OAuthState::Disconnected => "endpoint is disconnected",
            _ => unreachable!(),
        };
        return error_response(
            StatusCode::BAD_REQUEST,
            "cannot refresh tokens",
            Some(reason),
        )
        .into_response();
    }

    let inner = inner.clone();
    drop(inners_guard);

    // Attempt refresh
    match inner.do_token_refresh().await {
        Ok(new_tokens) => {
            let expires_at = new_tokens.expires_at;
            let refreshed_at = new_tokens.issued_at;
            inner.apply_tokens(new_tokens).await;

            let status = {
                let s = inner.state.read().await;
                format!("{:?}", *s)
            };

            Json(OAuthRefreshResponse {
                status,
                expires_at,
                refreshed_at,
            })
            .into_response()
        }
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            "token refresh failed",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the management API router with all /api routes.
pub fn management_routes(state: ManagementState) -> Router {
    Router::new()
        .route("/api/status", get(get_status))
        .route("/api/endpoints", get(get_endpoints))
        .route("/api/endpoints/{name}", delete(delete_endpoint))
        .route("/api/endpoints/{name}/tools", get(get_endpoint_tools))
        .route("/api/endpoints/{name}/restart", post(restart_endpoint))
        .route("/api/endpoints/{name}/refresh", post(refresh_endpoint))
        .route("/api/endpoints/{name}/disable", post(disable_endpoint))
        .route("/api/endpoints/{name}/enable", post(enable_endpoint))
        .route("/api/endpoints/{name}/logs", get(get_endpoint_logs))
        .route(
            "/api/endpoints/{name}/tools/{tool_name}/disable",
            post(disable_tool),
        )
        .route(
            "/api/endpoints/{name}/tools/{tool_name}/enable",
            post(enable_tool),
        )
        .route("/api/endpoints/{name}/oauth/start", post(oauth_start))
        .route("/api/endpoints/{name}/oauth/status", get(oauth_status))
        .route("/api/endpoints/{name}/oauth/revoke", post(oauth_revoke))
        .route("/api/endpoints/{name}/oauth/refresh", post(oauth_refresh))
        .route("/api/catalog", get(get_catalog))
        .route("/api/config", get(get_config))
        .route("/api/config/reload", post(reload_config))
        .route("/api/test-connection", post(test_connection))
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
        Ok(tools) => {
            let tools_with_status: Vec<ToolInfoWithStatus> = tools
                .into_iter()
                .map(|t| {
                    let disabled = entry.disabled_tools.contains(&t.name);
                    ToolInfoWithStatus {
                        name: t.name,
                        description: t.description,
                        input_schema: t.input_schema,
                        disabled,
                        annotations: t.annotations,
                    }
                })
                .collect();
            Json(tools_with_status).into_response()
        }
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to list tools",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolInfoWithStatus {
    name: String,
    description: Option<String>,
    input_schema: Value,
    disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<Value>,
}

/// GET /api/catalog
///
/// Returns the full merged/prefixed tool catalog across all endpoints,
/// including source endpoint name and availability status.
async fn get_catalog(State(state): State<ManagementState>) -> Json<Vec<CatalogEntry>> {
    let (tools, lookup) = state.registry.merged_catalog_with_lookup().await;
    let entries = state.registry.entries().read().await;
    let mut catalog = Vec::new();

    for tool in tools {
        let (endpoint_name, available) = match lookup.get(&tool.name) {
            Some((ep, _raw)) => {
                let avail = entries
                    .get(ep.as_str())
                    .map(|e| {
                        !e.disabled
                            && matches!(e.adapter.health(), crate::adapter::HealthStatus::Healthy)
                    })
                    .unwrap_or(false);
                (ep.clone(), avail)
            }
            None => ("unknown".to_string(), false),
        };

        catalog.push(CatalogEntry {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
            annotations: tool.annotations,
            endpoint: endpoint_name,
            available,
        });
    }

    Json(catalog)
}

/// POST /api/endpoints/:name/disable
async fn disable_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    {
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
        entry.disabled = true;
    }
    persist_disabled_state(&state).await;
    Json(ActionResponse {
        ok: true,
        message: format!("Endpoint '{}' disabled", name),
    })
    .into_response()
}

/// POST /api/endpoints/:name/enable
async fn enable_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let result = {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        entry.disabled = false;
        entry.adapter.initialize().await
    };
    persist_disabled_state(&state).await;
    match result {
        Ok(()) => Json(ActionResponse {
            ok: true,
            message: format!("Endpoint '{}' enabled", name),
        })
        .into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to initialize adapter",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

/// POST /api/endpoints/:name/tools/:tool_name/disable
async fn disable_tool(
    State(state): State<ManagementState>,
    Path((name, tool_name)): Path<(String, String)>,
) -> impl IntoResponse {
    {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        entry.disabled_tools.insert(tool_name.clone());
    }
    persist_disabled_state(&state).await;
    Json(ActionResponse {
        ok: true,
        message: format!("Tool '{}' disabled on '{}'", tool_name, name),
    })
    .into_response()
}

/// POST /api/endpoints/:name/tools/:tool_name/enable
async fn enable_tool(
    State(state): State<ManagementState>,
    Path((name, tool_name)): Path<(String, String)>,
) -> impl IntoResponse {
    {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        entry.disabled_tools.remove(&tool_name);
    }
    persist_disabled_state(&state).await;
    Json(ActionResponse {
        ok: true,
        message: format!("Tool '{}' enabled on '{}'", tool_name, name),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use crate::config::{Config, EndpointConfig, RelayConfig, Transport};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt; // for oneshot

    /// Mock adapter for testing.
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

        fn unhealthy(reason: &str) -> Self {
            Self {
                health: HealthStatus::Unhealthy(reason.to_string()),
                tools: vec![],
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
            Ok(Value::Null)
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
                env: Some(HashMap::from([(
                    "SECRET".to_string(),
                    "s3cret".to_string(),
                )])),
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
            }],
        }
    }

    async fn test_state(adapters: Vec<(&str, MockAdapter)>) -> ManagementState {
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
        ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(test_config())),
            start_time: Instant::now(),
            config_path: None,
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: None,
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
            annotations: None,
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
                annotations: None,
            },
            ToolInfo {
                name: "t2".into(),
                description: None,
                input_schema: serde_json::json!({}),
                annotations: None,
            },
        ];
        let state = test_state(vec![
            ("a", MockAdapter::healthy_with_tools(tools)),
            ("b", MockAdapter::unhealthy("down")),
        ])
        .await;
        let app = management_routes(state);
        let resp = app
            .oneshot(Request::get("/api/endpoints").body(Body::empty()).unwrap())
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
        assert_eq!(arr[1]["health"], "offline");
        assert_eq!(arr[1]["error"], "down");
        assert_eq!(arr[1]["tool_count"], 0);
    }

    #[tokio::test]
    async fn management_endpoint_tools() {
        let tools = vec![ToolInfo {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: None,
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
        assert!(
            arr[0].get("inputSchema").is_some(),
            "should use camelCase inputSchema"
        );
        assert!(
            arr[0].get("input_schema").is_none(),
            "should NOT use snake_case input_schema"
        );
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
        assert!(body["detail"].as_str().unwrap().contains("nonexistent"));
    }

    #[tokio::test]
    async fn management_endpoint_logs() {
        let mock = MockAdapter::healthy_with_tools(vec![])
            .with_stderr(vec!["line1".into(), "line2".into()]);
        let state = test_state(vec![("echo", mock)]).await;
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
            annotations: None,
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

    #[tokio::test]
    async fn management_delete_endpoint_success() {
        // Write a temp config file with two endpoints
        let dir = std::env::temp_dir().join(format!("relay-test-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_file = dir.join("config.toml");
        let toml_content = r#"[relay]
machine_name = "test"

[[endpoints]]
name = "echo"
transport = "stdio"
command = "echo"

[[endpoints]]
name = "keep-me"
transport = "stdio"
command = "cat"
"#;
        std::fs::write(&config_file, toml_content).unwrap();

        let mut state = test_state(vec![("echo", MockAdapter::healthy_with_tools(vec![]))]).await;
        state.config_path = Some(config_file.clone());
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::delete("/api/endpoints/echo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "removed");
        assert_eq!(body["name"], "echo");

        // Verify the config file was updated
        let updated = std::fs::read_to_string(&config_file).unwrap();
        assert!(!updated.contains("\"echo\""));
        assert!(updated.contains("keep-me"));

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn management_delete_endpoint_not_found() {
        let dir = std::env::temp_dir().join(format!("relay-test-delete-nf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_file = dir.join("config.toml");
        let toml_content = r#"[relay]
machine_name = "test"

[[endpoints]]
name = "echo"
transport = "stdio"
command = "echo"
"#;
        std::fs::write(&config_file, toml_content).unwrap();

        let mut state = test_state(vec![]).await;
        state.config_path = Some(config_file.clone());
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::delete("/api/endpoints/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("Endpoint not found"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn management_delete_endpoint_no_config_path() {
        let state = test_state(vec![]).await;
        // config_path is None
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::delete("/api/endpoints/echo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("config_path not configured"));
    }

    #[tokio::test]
    async fn management_disable_endpoint() {
        let tools = vec![ToolInfo {
            name: "t1".into(),
            description: None,
            input_schema: serde_json::json!({}),
            annotations: None,
        }];
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);

        // Disable the endpoint
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/echo/disable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("disabled"));

        // Verify GET /api/endpoints shows disabled=true and health=stopped
        let resp = app
            .oneshot(Request::get("/api/endpoints").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr[0]["disabled"], true);
        assert_eq!(arr[0]["health"], "stopped");
        assert_eq!(arr[0]["tool_count"], 0);
    }

    #[tokio::test]
    async fn management_enable_endpoint() {
        let tools = vec![ToolInfo {
            name: "t1".into(),
            description: None,
            input_schema: serde_json::json!({}),
            annotations: None,
        }];
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);

        // Disable first
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/echo/disable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Enable the endpoint
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/echo/enable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("enabled"));

        // Verify GET /api/endpoints shows disabled=false and health=healthy
        let resp = app
            .oneshot(Request::get("/api/endpoints").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr[0]["disabled"], false);
        assert_eq!(arr[0]["health"], "healthy");
    }

    #[tokio::test]
    async fn management_disable_not_found() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/missing/disable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn management_disable_tool() {
        let tools = vec![
            ToolInfo {
                name: "read".into(),
                description: Some("Read".into()),
                input_schema: serde_json::json!({"type": "object"}),
                annotations: None,
            },
            ToolInfo {
                name: "write".into(),
                description: Some("Write".into()),
                input_schema: serde_json::json!({"type": "object"}),
                annotations: None,
            },
        ];
        let state = test_state(vec![("fs", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);

        // Disable the "read" tool
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/fs/tools/read/disable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);

        // Verify GET /api/endpoints/fs/tools shows disabled=true for "read"
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
        let read_tool = arr.iter().find(|t| t["name"] == "read").unwrap();
        assert_eq!(read_tool["disabled"], true);
        assert!(
            read_tool.get("inputSchema").is_some(),
            "should use camelCase inputSchema"
        );
        assert!(
            read_tool.get("input_schema").is_none(),
            "should NOT use snake_case input_schema"
        );
        let write_tool = arr.iter().find(|t| t["name"] == "write").unwrap();
        assert_eq!(write_tool["disabled"], false);
    }

    #[tokio::test]
    async fn management_enable_tool() {
        let tools = vec![ToolInfo {
            name: "read".into(),
            description: Some("Read".into()),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: None,
        }];
        let state = test_state(vec![("fs", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state);

        // Disable then enable
        let _resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/fs/tools/read/disable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/fs/tools/read/enable")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);

        // Verify tool is no longer disabled
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/fs/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        let read_tool = arr.iter().find(|t| t["name"] == "read").unwrap();
        assert_eq!(read_tool["disabled"], false);
    }

    #[tokio::test]
    async fn management_test_connection_unknown_transport() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let body = serde_json::json!({
            "transport": "grpc"
        });
        let resp = app
            .oneshot(
                Request::post("/api/test-connection")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["success"], false);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("Unknown transport"));
    }

    #[tokio::test]
    async fn management_test_connection_stdio_bad_command() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let body = serde_json::json!({
            "transport": "stdio",
            "command": "/nonexistent/binary/that/does/not/exist"
        });
        let resp = app
            .oneshot(
                Request::post("/api/test-connection")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["success"], false);
        assert!(body["error"].is_string());
    }

    // --- OAuth management route tests ---

    use crate::adapter::oauth::{OAuthAdapter, OAuthAdapterConfig, OAuthAdapterInner, OAuthState};
    use crate::token_manager::TokenManager;

    fn make_oauth_config(name: &str) -> OAuthAdapterConfig {
        OAuthAdapterConfig {
            endpoint_name: name.to_string(),
            url: "http://127.0.0.1:19999/mcp".to_string(),
            token_endpoint_url: "http://127.0.0.1:19999/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
        }
    }

    async fn test_state_with_oauth(
        name: &str,
        tmp_dir: &std::path::Path,
    ) -> (ManagementState, Arc<OAuthAdapterInner>) {
        let token_manager = Arc::new(TokenManager::new(tmp_dir.to_path_buf()));
        let config = make_oauth_config(name);
        let adapter = OAuthAdapter::new(config, token_manager.clone());
        let shared_inner = adapter.shared_inner();

        let oauth_inners: OAuthAdapterInners =
            Arc::new(RwLock::new(std::collections::HashMap::new()));
        oauth_inners
            .write()
            .await
            .insert(name.to_string(), shared_inner.clone());

        let registry = AdapterRegistry::new();
        registry
            .register(
                name.to_string(),
                Box::new(adapter),
                "oauth".to_string(),
                None,
                None,
            )
            .await;

        let state = ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(test_config())),
            start_time: Instant::now(),
            config_path: None,
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: Some(oauth_inners),
        };

        (state, shared_inner)
    }

    #[tokio::test]
    async fn oauth_status_detailed_needs_login() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::get("/api/endpoints/oauth-ep/oauth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "needs_login");
        assert_eq!(body["has_access_token"], false);
        assert_eq!(body["has_refresh_token"], false);
    }

    #[tokio::test]
    async fn oauth_status_detailed_with_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;

        // Set up tokens with expiry
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token_set = crate::token_manager::TokenSet {
            access_token: "test-access".to_string(),
            refresh_token: Some("test-refresh".to_string()),
            expires_at: Some(now + 3600),
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: Some(now),
        };
        *inner.tokens.write().await = Some(token_set);
        *inner.state.write().await = OAuthState::Authenticated;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/oauth-ep/oauth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "authenticated");
        assert_eq!(body["has_access_token"], true);
        assert_eq!(body["has_refresh_token"], true);
        assert!(body["expires_at"].is_number());
        assert!(body["expires_in_seconds"].is_number());
        assert!(body["last_refreshed_at"].is_number());
        assert!(body["next_refresh_at"].is_number());
    }

    #[tokio::test]
    async fn oauth_status_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::get("/api/endpoints/nonexistent/oauth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_revoke_success() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;

        // Set authenticated state
        *inner.state.write().await = OAuthState::Authenticated;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/oauth-ep/oauth/revoke")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "disconnected");
        assert_eq!(body["endpoint"], "oauth-ep");

        // Verify state changed to Disconnected
        let state = inner.state.read().await;
        assert_eq!(*state, OAuthState::Disconnected);
    }

    #[tokio::test]
    async fn oauth_revoke_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/nonexistent/oauth/revoke")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_revoke_not_oauth_endpoint() {
        // Non-OAuth endpoint with no adapter inners
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(vec![]))]).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/oauth/revoke")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oauth_refresh_needs_login_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        // State is NeedsLogin by default
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/oauth-ep/oauth/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert!(body["detail"]
            .as_str()
            .unwrap()
            .contains("never been authenticated"));
    }

    #[tokio::test]
    async fn oauth_refresh_disconnected_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        *inner.state.write().await = OAuthState::Disconnected;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/oauth-ep/oauth/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert!(body["detail"].as_str().unwrap().contains("disconnected"));
    }

    #[tokio::test]
    async fn oauth_refresh_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/nonexistent/oauth/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_refresh_no_refresh_token_returns_502() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;

        // Set Authenticated state but with no refresh token
        *inner.state.write().await = OAuthState::Authenticated;
        *inner.tokens.write().await = Some(crate::token_manager::TokenSet {
            access_token: "test-access".to_string(),
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: None,
            issued_at: None,
        });

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/oauth-ep/oauth/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Should return 502 because refresh fails (no refresh token)
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
