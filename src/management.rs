use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::RwLock;

use tracing::{info, warn};

use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::oauth::{OAuthState, RefreshCommitOutcome};
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::{FailedAdapter, HealthStatus, McpAdapter, StartingAdapter};
use crate::config::{Config, ObservabilityConfig};
use crate::events::ToolCallEventBus;
use crate::oauth::client::{self, ClientRegistration};
use crate::oauth::{
    append_google_authorize_params, OAuthFlowManager, OAuthSetupManager, PkceChallenge,
};
use crate::observability::payloads::StoredPayloads;
use crate::observability::store::{AggregateBucket, CallRecord, QueryFilter};
use crate::profile_registry::ProfileRegistry;
use crate::registry::AdapterRegistry;
use crate::token_manager::{
    dcr_issuer_allows_reuse, merge_scopes, DcrCredentials, TokenError, TokenManager,
};
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
    /// Token manager for DCR credential persistence.
    pub token_manager: Option<Arc<TokenManager>>,
    /// Transient OAuth setup session manager (preflight flow).
    pub setup_manager: Option<Arc<OAuthSetupManager>>,
    /// Relay-wide profile registry. Profile CRUD handlers call
    /// [`ProfileRegistry::rebuild`] directly after a successful TOML
    /// writeback so the live `/mcp/{profile}` routes reflect the change
    /// without waiting on `ConfigWatcher` (the watcher path is owned by
    /// R4.B). `None` is permitted for legacy test fixtures that don't
    /// exercise the profile routes.
    pub profile_registry: Option<Arc<ProfileRegistry>>,
    /// Shared typed tool-call event bus consumed by the desktop overlay's
    /// SSE stream at `GET /api/events/tool-calls`. Adapters publish
    /// `started` / `completed` / `failed` events via the same bus. `None`
    /// is permitted for unit-test fixtures that do not exercise the
    /// overlay route; in that case the SSE handler returns 503.
    pub event_bus: Option<ToolCallEventBus>,
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
    /// Whether a container runtime (docker/podman) was detected on the host.
    /// `null` while background detection is still in flight; consumers should
    /// only treat an explicit `false` as "no runtime available".
    pub container_runtime_available: Option<bool>,
}

/// Cached result of container runtime detection, warmed off the async
/// runtime by [`management_routes`] so the first `/api/status` call never
/// blocks on shell/CLI probing.
static CONTAINER_RUNTIME_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Kick off container runtime detection on a background thread (idempotent;
/// the detector itself caches for the process lifetime).
fn warm_container_runtime_detection() {
    if CONTAINER_RUNTIME_AVAILABLE.get().is_some() {
        return;
    }
    std::thread::spawn(|| {
        let available = crate::container_runtime::detect_runtime().is_some();
        let _ = CONTAINER_RUNTIME_AVAILABLE.set(available);
    });
}

/// Lifecycle state for an adapter, surfaced in the management API.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state")]
pub enum Lifecycle {
    /// Adapter is initializing (handshake in progress).
    Initializing,
    /// Adapter is ready and healthy.
    Ready {
        /// Effective server name (after `server_type_override` resolution),
        /// derived from the sanitized MCP `serverInfo.name`.
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name: Option<String>,
        /// Upstream-reported server name (sanitized + suffix-stripped),
        /// independent of any `server_type_override`. Equals `server_name`
        /// when no override is configured. Surfaced so the desktop UI can
        /// show the user the default they would revert to.
        #[serde(skip_serializing_if = "Option::is_none")]
        server_name_raw: Option<String>,
    },
    /// Adapter failed to initialize or is unhealthy.
    Failed {
        /// Error details.
        error: LifecycleError,
    },
    /// Adapter is stopped/disabled.
    Stopped,
}

/// Error details for a failed adapter.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleError {
    /// Error kind (e.g., "ServerName", "Transport", "Protocol").
    pub kind: String,
    /// Human-readable error detail.
    pub detail: String,
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
    /// Lifecycle state of the adapter.
    pub lifecycle: Lifecycle,
    /// Latest resource stats sample for containerized stdio endpoints.
    /// Omitted for direct-spawn endpoints and when no sample is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_stats: Option<crate::container_stats::ContainerStats>,
    /// Isolation outcome (configured vs actual) of the last spawn for stdio
    /// endpoints. Omitted for non-stdio transports and before the first spawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation_state: Option<crate::adapter::stdio::IsolationState>,
    /// EMA org-binding summary for endpoints authenticated via Enterprise-Managed
    /// Authorization (`[endpoints.auth] type = "ema"`). Lets the desktop detect
    /// already-installed org-bound servers. Omitted for ordinary endpoints,
    /// keeping their serialization byte-for-byte unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<EmaAuthSummary>,
}

/// EMA org-binding summary surfaced on an endpoint listing entry.
///
/// Populated from the endpoint's `[endpoints.auth]` block when `type = "ema"`.
/// Carries no credentials — only the org reference and the MCP server URL the
/// minted access token is scoped to.
#[derive(Serialize, Clone)]
pub struct EmaAuthSummary {
    /// Auth scheme discriminator; currently always `"ema"`.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Name of the `[[organizations]]` entry this endpoint binds to. Omitted for
    /// END-18 bare-`idp` endpoints with no organization reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// MCP server URL the EMA access token is minted for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
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
        container_runtime_available: CONTAINER_RUNTIME_AVAILABLE.get().copied(),
    })
}

/// GET /api/endpoints
async fn get_endpoints(State(state): State<ManagementState>) -> Json<Vec<EndpointInfo>> {
    // Snapshot EMA org bindings from config, keyed by endpoint name. Only
    // `type = "ema"` endpoints get an entry; ordinary endpoints are absent so
    // their listing serialization stays byte-for-byte unchanged.
    let auth_by_name: HashMap<String, EmaAuthSummary> = {
        let config = state.config.read().await;
        config
            .endpoints
            .iter()
            .filter_map(|ep| {
                let auth = ep.auth.as_ref()?;
                if auth.auth_type != "ema" {
                    return None;
                }
                Some((
                    ep.name.clone(),
                    EmaAuthSummary {
                        auth_type: auth.auth_type.clone(),
                        organization: auth.organization.clone(),
                        resource: auth.resource.clone(),
                    },
                ))
            })
            .collect()
    };
    let entries = state.registry.entries().read().await;
    let now = Instant::now();
    let mut endpoints: Vec<EndpointInfo> = Vec::new();
    for (name, entry) in entries.iter() {
        let (health, tool_count, error, lifecycle) = if entry.disabled {
            ("stopped".to_string(), 0, None, Lifecycle::Stopped)
        } else {
            match entry.adapter.health() {
                HealthStatus::Healthy => {
                    let count = entry
                        .cached_list_tools()
                        .await
                        .map(|t| t.len())
                        .unwrap_or(0);
                    let server_name = entry.adapter.server_type();
                    let server_name_raw = entry.adapter.upstream_server_name();
                    (
                        "healthy".to_string(),
                        count,
                        None,
                        Lifecycle::Ready {
                            server_name,
                            server_name_raw,
                        },
                    )
                }
                HealthStatus::Unhealthy(reason) => {
                    let lifecycle = Lifecycle::Failed {
                        error: LifecycleError {
                            kind: categorize_error(&reason),
                            detail: reason.clone(),
                        },
                    };
                    ("offline".to_string(), 0, Some(reason), lifecycle)
                }
                HealthStatus::Starting => {
                    ("starting".to_string(), 0, None, Lifecycle::Initializing)
                }
                HealthStatus::Stopped => ("stopped".to_string(), 0, None, Lifecycle::Stopped),
            }
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
            lifecycle,
            container_stats: entry.adapter.container_stats(),
            isolation_state: entry.adapter.isolation_state(),
            auth: auth_by_name.get(name).cloned(),
        });
    }
    endpoints.sort_by(|a, b| a.name.cmp(&b.name));
    Json(endpoints)
}

/// Categorize an error message into a kind for the lifecycle response.
fn categorize_error(reason: &str) -> String {
    if reason.contains("serverInfo.name") || reason.contains("ServerNameError") {
        "ServerName".to_string()
    } else if reason.contains("transport")
        || reason.contains("connection")
        || reason.contains("Connection")
    {
        "Transport".to_string()
    } else if reason.contains("timeout") || reason.contains("Timeout") {
        "Timeout".to_string()
    } else if reason.contains("protocol") || reason.contains("Protocol") {
        "Protocol".to_string()
    } else {
        "Unknown".to_string()
    }
}

/// POST /api/endpoints/:name/restart
///
/// Returns immediately after validating the endpoint exists and swapping in a
/// `StartingAdapter` placeholder. The slow shutdown + re-initialize work runs
/// in a background task that swaps the freshly-built adapter back into the
/// registry on completion. Failures leave the registry entry as a
/// `FailedAdapter` so the standard health channel surfaces the error.
///
/// Emits `tools_changed_tx` ticks on both the foreground placeholder swap
/// (catalog briefly loses the endpoint's tools while `StartingAdapter` is in
/// place) and the background re-init swap completion (catalog regains the
/// rebuilt adapter's tools). Both ticks mirror what subscribers reading the
/// catalog would actually observe at each phase; emitting only the second
/// would hide the transient "tools gone" state. This is independent of any
/// tick the rebuilt adapter's upstream may later emit via
/// `rewire_tools_changed_listener` — that path handles real upstream
/// `tools/list_changed` events, while these two ticks reflect the relay-side
/// swap itself.
async fn restart_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Validate the endpoint exists and atomically swap in a placeholder so the
    // request returns immediately without awaiting shutdown/initialize.
    let old_adapter: Box<dyn McpAdapter> = {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        std::mem::replace(&mut entry.adapter, Box::new(StartingAdapter))
    };
    state.registry.invalidate_catalog_cache().await;
    // Foreground tick: the catalog has just lost the endpoint's tools. Send
    // after `invalidate_catalog_cache` so subscribers re-reading the catalog
    // on receipt see the placeholder state.
    state.registry.tick_tools_changed(&name);

    // Arc-clone everything the background task needs so it owns its captures.
    let registry = state.registry.clone();
    let config = state.config.clone();
    let token_manager = state.token_manager.clone();
    let oauth_adapter_inners = state.oauth_adapter_inners.clone();
    let event_bus = state.event_bus.clone();
    // Captured for JIT wiring of a rebuilt plain-`http` adapter (mirrors the
    // initial-load and hot-reload paths). The loopback redirect_uri uses the
    // live relay_port; if no flow manager is configured, JIT stays dormant.
    let oauth_flow_manager = state.oauth_flow_manager.clone();
    let relay_port = state.relay_port;
    let task_name = name.clone();

    tokio::spawn(async move {
        tracing::info!(endpoint = %task_name, "Restart: background task started");

        // Shut down the old adapter; log but don't propagate errors — the
        // restart must converge regardless of shutdown outcome.
        let mut old = old_adapter;
        if let Err(e) = old.shutdown().await {
            tracing::warn!(
                endpoint = %task_name,
                error = %e,
                "Restart: shutdown of previous adapter failed"
            );
        }

        // Look up the endpoint config to decide between rebuild and re-init.
        let (ep_config, allow_insecure_oauth, organizations) = {
            let cfg = config.read().await;
            (
                cfg.endpoints
                    .iter()
                    .find(|ep| ep.name == task_name)
                    .cloned(),
                cfg.relay.allow_insecure_oauth.unwrap_or(false),
                cfg.organizations.clone(),
            )
        };

        let new_adapter: Box<dyn McpAdapter> = if let Some(ep) = ep_config {
            let tm = token_manager.unwrap_or_else(|| {
                Arc::new(crate::token_manager::TokenManager::new(PathBuf::from(
                    "/tmp",
                )))
            });
            let oai = oauth_adapter_inners.unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new())));
            let jit = oauth_flow_manager
                .as_ref()
                .map(|fm| crate::watcher::JitWiring {
                    relay_port,
                    flow_manager: fm.clone(),
                });
            crate::watcher::create_adapter(
                &ep,
                &tm,
                &oai,
                allow_insecure_oauth,
                event_bus.as_ref(),
                jit.as_ref(),
                &organizations,
            )
            .await
        } else {
            // Endpoint not in config: re-initialize the previous adapter in
            // place. On failure, surface the error via FailedAdapter so the
            // standard health channel reports it.
            match old.initialize().await {
                Ok(()) => old,
                Err(e) => {
                    tracing::error!(
                        endpoint = %task_name,
                        error = %e,
                        "Restart: failed to re-initialize adapter not in config"
                    );
                    Box::new(FailedAdapter::new(e.to_string()))
                }
            }
        };

        // Swap the new adapter back into the registry.
        {
            let mut entries = registry.entries().write().await;
            if let Some(entry) = entries.get_mut(&task_name) {
                entry.adapter = new_adapter;
            } else {
                tracing::warn!(
                    endpoint = %task_name,
                    "Restart: endpoint disappeared from registry before swap"
                );
            }
        }
        // Rewire the tools-changed listener against the freshly swapped
        // adapter, and invalidate any per-endpoint cached tool list so the
        // next /tools/list reflects the rebuilt adapter.
        registry.rewire_tools_changed_listener(&task_name).await;
        registry.invalidate_endpoint_tool_cache(&task_name).await;
        registry.invalidate_catalog_cache().await;
        // Background tick: the catalog has just regained the endpoint's
        // tools via the rebuilt adapter. Sent after cache invalidation so
        // subscribers re-reading the catalog see the post-swap state. This
        // is distinct from any future tick emitted by the rewired listener
        // when the upstream server itself sends `tools/list_changed`.
        registry.tick_tools_changed(&task_name);

        tracing::info!(endpoint = %task_name, "Restart: background task complete");
    });

    Json(ActionResponse {
        ok: true,
        message: format!("Endpoint '{}' restarted", name),
    })
    .into_response()
}

/// POST /api/endpoints/:name/refresh
async fn refresh_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Drop the prior cached tools so the next read fetches fresh data,
    // then repopulate the cache eagerly so callers see the new list.
    state.registry.invalidate_endpoint_tool_cache(&name).await;
    let entries = state.registry.entries().read().await;
    let Some(entry) = entries.get(&name) else {
        return endpoint_not_found(&name).into_response();
    };
    let result = entry.cached_list_tools().await;
    drop(entries);
    match result {
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
    /// Agent-call observability settings (`[relay.observability]`). Exposed so
    /// the desktop Settings tab can render the Observability section; contains
    /// no secrets, so it is surfaced verbatim.
    observability: ObservabilityConfig,
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
            observability: config.relay.observability.clone(),
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
async fn reload_config(State(state): State<ManagementState>) -> Json<ActionResponse> {
    let Some(config_path) = &state.config_path else {
        return Json(ActionResponse {
            ok: false,
            message: "config_path not configured".to_string(),
        });
    };

    let resolved = crate::config::expand_tilde(config_path);

    // Parse new config from disk
    let (new_config, warnings) = match crate::config::load_config_graceful(&resolved) {
        Ok(result) => result,
        Err(e) => {
            return Json(ActionResponse {
                ok: false,
                message: format!("failed to parse config: {}", e),
            });
        }
    };

    for w in &warnings {
        tracing::warn!("{}", w);
    }
    let warned_names = crate::config::warned_endpoint_names(&warnings);

    // Diff against current in-memory config
    let old_config = state.config.read().await;
    let diff = crate::config::diff_configs(&old_config, &new_config);
    drop(old_config);

    // Apply diff to registry
    let token_manager = state.token_manager.clone().unwrap_or_else(|| {
        Arc::new(crate::token_manager::TokenManager::new(
            std::path::PathBuf::from("/tmp"),
        ))
    });
    let oauth_adapter_inners = state
        .oauth_adapter_inners
        .clone()
        .unwrap_or_else(|| Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())));

    let jit = state
        .oauth_flow_manager
        .as_ref()
        .map(|fm| crate::watcher::JitWiring {
            relay_port: state.relay_port,
            flow_manager: fm.clone(),
        });
    crate::watcher::apply_diff_graceful(
        &diff,
        &state.registry,
        &warnings,
        &warned_names,
        &token_manager,
        &oauth_adapter_inners,
        new_config.relay.allow_insecure_oauth.unwrap_or(false),
        state.event_bus.as_ref(),
        jit.as_ref(),
        &new_config.organizations,
    )
    .await;

    // Update in-memory config baseline
    *state.config.write().await = new_config;

    Json(ActionResponse {
        ok: true,
        message: "config reloaded".to_string(),
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

    if let Err(e) = crate::config::write_config_file(&resolved, &new_contents) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write config file",
            Some(&e.to_string()),
        )
        .into_response();
    }

    // R6: cascade observability cleanup for the deleted server. The metadata
    // store is keyed by `server_name`, but the in-memory payload buffer is
    // keyed by `request_uid` — so derive the request_uids for this server from
    // the metadata rows BEFORE deleting them, then delete the rows and drop the
    // matching buffered payloads. No-op when observability is unwired/disabled.
    if let Some(obs) = state.registry.observability().filter(|o| o.is_enabled()) {
        // Exact-match collection (`WHERE server_name = ?1`) so a sibling server
        // whose name contains this one as a substring (e.g. `foo-staging` when
        // deleting `foo`) is not over-collected. SQLite work runs on a blocking
        // thread so it never stalls the async runtime.
        let store = Arc::clone(obs.store());
        let server = name.clone();
        let cascade = tokio::task::spawn_blocking(move || {
            let uids = store.request_uids_for_server(&server);
            let removed = store.delete_for_server(&server);
            (uids, removed)
        })
        .await;
        match cascade {
            Ok((uids, removed)) => {
                match removed {
                    Ok(removed) => {
                        tracing::debug!(server = %name, removed, "observability: deleted metadata rows for deleted server")
                    }
                    Err(e) => {
                        warn!(error = %e, server = %name, "observability: failed to delete metadata rows for deleted server")
                    }
                }
                match uids {
                    Ok(uids) => {
                        if !uids.is_empty() {
                            let uids: std::collections::HashSet<String> =
                                uids.into_iter().collect();
                            obs.payloads().remove_for_server(&uids);
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, server = %name, "observability: failed to collect request_uids for delete cascade")
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, server = %name, "observability: delete cascade task panicked")
            }
        }
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

/// Read disabled/disabled_tools from the registry, mirror them into the
/// in-memory config baseline, and write them back to config.toml.
///
/// The on-disk write is a targeted edit: the file is re-read and parsed into
/// a raw `toml::Table`, only the `disabled` / `disabled_tools` keys on the
/// matching `[[endpoints]]` entries (by `name`) are updated, and the table is
/// reserialized via [`crate::config::write_config_file`]. Sections and keys
/// the typed [`Config`] struct does not model (e.g. `[desktop]`, `[meta]`)
/// are retained semantically rather than verbatim: `toml::to_string_pretty`
/// reserializes the whole document, so comments and formatting are lost and
/// sections may be reordered (the same caveat as
/// `update_observability_config`). Endpoints absent from the file are not
/// re-added.
async fn persist_disabled_state(state: &ManagementState) {
    let Some(ref config_path) = state.config_path else {
        return;
    };
    // Hold the config write lock across the file write so concurrent
    // persists cannot interleave their read-modify-write cycles.
    let mut config = state.config.write().await;

    // Read current disabled state from registry and update the in-memory
    // baseline so GET handlers reflect the change immediately.
    let entries = state.registry.entries().read().await;
    for ep_config in &mut config.endpoints {
        if let Some(entry) = entries.get(&ep_config.name) {
            ep_config.disabled = entry.disabled;
            ep_config.disabled_tools = entry.disabled_tools.iter().cloned().collect();
        }
    }
    drop(entries);

    // Targeted on-disk edit so unknown sections/keys survive.
    let resolved = crate::config::expand_tilde(config_path);
    let contents = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to persist disabled state: cannot read config file");
            return;
        }
    };
    let mut parsed: toml::Table = match contents.parse() {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "Failed to persist disabled state: cannot parse config file");
            return;
        }
    };
    if let Some(toml::Value::Array(endpoints)) = parsed.get_mut("endpoints") {
        for ep in endpoints.iter_mut() {
            let Some(tbl) = ep.as_table_mut() else {
                continue;
            };
            let Some(name) = tbl.get("name").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            let Some(ep_config) = config.endpoints.iter().find(|e| e.name == name) else {
                continue;
            };
            // `disabled` and `disabled_tools` are `#[serde(default)]` without
            // `skip_serializing_if` on `EndpointConfig`, so the typed
            // serializer always writes them; match that convention here.
            tbl.insert(
                "disabled".to_string(),
                toml::Value::Boolean(ep_config.disabled),
            );
            tbl.insert(
                "disabled_tools".to_string(),
                toml::Value::Array(
                    ep_config
                        .disabled_tools
                        .iter()
                        .cloned()
                        .map(toml::Value::String)
                        .collect(),
                ),
            );
        }
    }
    let new_contents = match toml::to_string_pretty(&parsed) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "Failed to persist disabled state: cannot serialize config");
            return;
        }
    };
    if let Err(e) = crate::config::write_config_file(&resolved, &new_contents) {
        warn!(error = %e, "Failed to persist disabled state");
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
                ..Default::default()
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

/// Response for POST /api/endpoints/:name/oauth/start (success)
#[derive(Serialize)]
struct OAuthStartResponse {
    authorize_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    discovery: Option<OAuthDiscoveryInfo>,
}

/// Informational discovery metadata included in /oauth/start response.
#[derive(Serialize)]
struct OAuthDiscoveryInfo {
    auth_server: String,
    dcr_used: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes_available: Vec<String>,
}

/// Error response when DCR is unsupported and manual credentials are needed.
#[derive(Serialize)]
struct OAuthDcrUnsupportedResponse {
    error: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes_supported: Vec<String>,
}

/// Request body for POST /api/endpoints/:name/oauth/credentials
#[derive(Deserialize)]
struct OAuthCredentialsRequest {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

/// Request body for POST /api/endpoints/:name/credentials.
///
/// All fields are optional in the JSON shape so callers can omit secrets they
/// don't want to update. `client_id` is required at runtime — if missing, the
/// handler responds with 400.
#[derive(Deserialize)]
struct EndpointCredentialsRequest {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    /// Currently informational — `DcrCredentials` does not yet persist this
    /// field, so it's accepted for forward compatibility but ignored.
    #[serde(default)]
    #[allow(dead_code)]
    oauth_server_url: Option<String>,
    /// Optional EMA **resource** `client_id` presented at the MCP Authorization
    /// Server in Step 3 (ID-JAG redemption) — R3 re-scoped this pair from the
    /// org record to the endpoint because it is per-resource. Absent preserves
    /// the stored value, an empty string clears it, a non-empty value sets it.
    /// Persisted in `{name}.dcr.json` (0600), never in `config.toml`.
    #[serde(default)]
    resource_client_id: Option<String>,
    /// Optional EMA **resource** `client_secret` paired with `resource_client_id`,
    /// presented via `client_secret_post` at the MAS in Step 3. Same merge
    /// semantics; never written to `config.toml` and never returned to the UI.
    #[serde(default)]
    resource_client_secret: Option<String>,
}

/// Response body for GET /api/endpoints/:name/credentials.
#[derive(Serialize)]
struct EndpointCredentialsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    client_secret_set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_server_url: Option<String>,
    /// The EMA **resource** `client_id` stored per-endpoint (R3); omitted when
    /// unset. The paired secret is never returned — only its presence via
    /// `resource_client_secret_set`.
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_client_id: Option<String>,
    resource_client_secret_set: bool,
    /// Where the credentials surfaced from: "dcr" or "config" or "none".
    source: &'static str,
}

/// Response for GET /api/endpoints/:name/oauth/status (simple)
#[derive(Serialize)]
struct OAuthStatusResponse {
    status: String,
}

/// A single entry in the transition history returned by the management API.
#[derive(Serialize)]
struct TransitionHistoryEntry {
    from: String,
    to: String,
    reason: String,
    ago_ms: u64,
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
    transition_history: Vec<TransitionHistoryEntry>,
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

/// Optional query parameters for POST /api/endpoints/:name/oauth/start.
#[derive(Deserialize, Default)]
struct OAuthStartQuery {
    /// When `true`, append `prompt=consent` to the authorize URL so the
    /// provider re-shows its consent screen instead of silently reusing the
    /// prior grant. Non-OIDC providers ignore unknown parameters, so this is
    /// safe to send everywhere.
    #[serde(default)]
    force_consent: bool,
}

/// POST /api/endpoints/:name/oauth/start
///
/// Generates a PKCE challenge, registers a pending flow, and returns the
/// authorization URL that the user should open in a browser.
async fn oauth_start(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
    Query(query): Query<OAuthStartQuery>,
) -> impl IntoResponse {
    oauth_start_inner(state, name, query.force_consent).await
}

/// Shared implementation of the auth-start flow, used by `oauth_start` and by
/// `oauth_reset` (which forces consent).
///
/// Resolution order:
/// 1. If `oauth_server_url` is in config, derive endpoints from convention.
///    Otherwise, try RFC 9728 discovery against the endpoint URL.
/// 2. If `client_id` is in config, use it. Otherwise, load persisted DCR
///    credentials → if missing/expired + registration_endpoint available,
///    attempt dynamic client registration → if DCR fails/unavailable, return
///    `dcr_unsupported` so the UI can prompt for manual credentials.
///
/// `force_consent` appends `prompt=consent` to the composed authorize URL.
async fn oauth_start_inner(
    state: ManagementState,
    name: String,
    force_consent: bool,
) -> axum::response::Response {
    use crate::oauth::dcr;
    use crate::oauth::discovery;

    // Look up endpoint config
    let config = state.config.read().await;
    let ep = config.endpoints.iter().find(|e| e.name == name);
    let Some(ep) = ep else {
        return endpoint_not_found(&name).into_response();
    };

    let oauth_server_url = ep.oauth_server_url.clone();
    let config_client_id = ep.client_id.clone();
    let config_client_secret = ep.client_secret.clone();
    let scopes = ep.scopes.clone();
    let config_token_endpoint = ep.token_endpoint.clone();
    let endpoint_url = ep.url.clone().unwrap_or_default();
    let allow_insecure_oauth = config.relay.allow_insecure_oauth.unwrap_or(false);
    drop(config);

    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            Some("OAuth flow manager not initialized"),
        )
        .into_response();
    };

    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);

    // Sample the endpoint's reset generation BEFORE the network-bound
    // discovery/DCR work below. A "Reset authorization" that lands while
    // this start is mid-discovery cannot invalidate it (no pending flow
    // exists yet), so the flow registration at Step 3 re-checks this value
    // and refuses the insert if a reset bumped it — otherwise this start
    // would hand out a post-reset-valid authorize URL WITHOUT the reset's
    // `prompt=consent`, silently reusing the old provider grant.
    let start_generation = flow_mgr.generation(&name).await;

    // ── Step 1: Resolve OAuth server metadata ──────────────────────────
    let (
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        discovered_scopes,
        auth_server_label,
        issuer,
        iss_supported,
    ) = if let Some(ref server_url) = oauth_server_url {
        // Prefer RFC 8414 discovery against the configured AS URL. If it
        // succeeds, use the discovered endpoints (explicit token_endpoint
        // config still wins). On a 404-class failure (metadata genuinely
        // absent), fall back to the legacy convention-based construction so
        // behavior is unchanged for servers that don't expose AS metadata.
        // On a transient failure (unreachable / timed out) do NOT guess —
        // the server likely publishes metadata and the composed
        // `{base}/authorize` URL would send the user to a dead page.
        match discovery::discover_authorization_server(server_url, allow_insecure_oauth).await {
            Ok(disc) => {
                let token_url = config_token_endpoint
                    .clone()
                    .unwrap_or_else(|| disc.token_endpoint.clone());
                (
                    disc.authorization_endpoint,
                    token_url,
                    disc.registration_endpoint,
                    disc.scopes_supported,
                    Some(disc.auth_server_url),
                    Some(disc.issuer),
                    disc.authorization_response_iss_parameter_supported,
                )
            }
            Err(e) if e.is_transient() => {
                warn!(
                    endpoint = %name,
                    error = %e,
                    "RFC 8414 discovery against oauth_server_url failed transiently; not composing a convention-based authorize URL"
                );
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "discovery_unreachable",
                    Some(&format!(
                        "Could not reach the OAuth server to discover its endpoints. \
                             Check connectivity and try again. Details: {e}"
                    )),
                )
                .into_response();
            }
            Err(
                e @ (discovery::DiscoveryError::MetadataNotFound { .. }
                | discovery::DiscoveryError::AuthServerMetadataNotFound { .. }),
            ) => {
                // 404-class only: metadata is genuinely absent, so the legacy
                // convention-based construction is the best we can do.
                warn!(
                    endpoint = %name,
                    error = %e,
                    "RFC 8414 discovery against oauth_server_url found no metadata; falling back to convention-based endpoints"
                );
                let base = server_url.trim_end_matches('/');
                let token_url = config_token_endpoint
                    .clone()
                    .unwrap_or_else(|| format!("{}/token", base));
                (
                    format!("{}/authorize", base),
                    token_url,
                    None::<String>,
                    Vec::<String>::new(),
                    None::<String>,
                    None::<String>,
                    false,
                )
            }
            Err(e) => {
                // Any other failure (non-404 HTTP status, malformed metadata,
                // S256 unsupported, URL-policy rejection) does not establish
                // that metadata is absent — composing `{base}/authorize`
                // could produce the same dead or unsafe redirect.
                warn!(
                    endpoint = %name,
                    error = %e,
                    "RFC 8414 discovery against oauth_server_url failed; not composing a convention-based authorize URL"
                );
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "discovery_failed",
                    Some(&format!(
                        "Could not discover OAuth server endpoints from \
                             oauth_server_url. Fix the server metadata or the \
                             configured URL and try again. Details: {e}"
                    )),
                )
                .into_response();
            }
        }
    } else {
        // Try RFC 9728 discovery (URL guard + per-host pinned client live inside)
        match discovery::discover_oauth_server(&endpoint_url, allow_insecure_oauth).await {
            Ok(disc) => (
                disc.authorization_endpoint,
                disc.token_endpoint,
                disc.registration_endpoint,
                disc.scopes_supported,
                Some(disc.auth_server_url),
                Some(disc.issuer),
                disc.authorization_response_iss_parameter_supported,
            ),
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "discovery_failed",
                    Some(&format!(
                        "Could not discover OAuth server for this endpoint. \
                             Configure oauth_server_url manually, or ensure the server \
                             supports RFC 9728. Details: {e}"
                    )),
                )
                .into_response();
            }
        }
    };

    // ── Step 2: Resolve client credentials ─────────────────────────────
    // Always attempt to load DCR credentials so that endpoints whose
    // `client_id` lives in TOML but whose `client_secret` is persisted only
    // in the DCR file (chmod 0600, written by `add_endpoint` /
    // `oauth_setup_credentials`) re-authorize correctly. This mirrors
    // `watcher::resolve_oauth_client_creds`, which already prefers DCR.
    let persisted_dcr = if let Some(ref tm) = state.token_manager {
        match tm.load_dcr(&name).await {
            Ok(Some(creds)) => Some(creds),
            Ok(None) => None,
            Err(e) => {
                warn!(endpoint = %name, error = %e, "Failed to load DCR credentials");
                None
            }
        }
    } else {
        None
    };

    // Snapshot of the on-disk requesting `client_id` at load time.
    // Used by R4-4's fresh-registration compare-and-update below so the
    // save's expected value corresponds to what was actually on disk
    // when we read it — the issuer-mismatch tombstone transformation
    // below rewrites `persisted_dcr.client_id` to `""` in memory only
    // and must NOT be treated as the on-disk snapshot for the compare.
    let persisted_dcr_disk_client_id: Option<String> = persisted_dcr
        .as_ref()
        .filter(|c| c.registered_via_dcr)
        .map(|c| c.client_id.clone());

    // Credential-to-issuer binding: if the stored DCR credentials were
    // registered against a DIFFERENT authorization server issuer than the one
    // we just discovered, invalidate them so resolution falls through to
    // dynamic re-registration (RFC 7591). Legacy creds with no stored issuer
    // are reused as-is (backward compatibility).
    //
    // Only the issuer-bound *requesting* pair
    // (`client_id`/`client_secret`/`issuer`) is invalidated: the operator-set
    // `resource_client_id`/`resource_client_secret` pair is a distinct
    // registration at the MCP Authorization Server (Step 3, RFC 7523
    // ID-JAG redemption) that has no dependency on the requesting client's
    // IdP issuer, and must be carried forward. Converting to `None` here
    // and letting the fresh-registration branch below build the record with
    // `..Default::default()` would silently erase the operator's manual
    // resource pair (round-4 finding R4-1). Instead we hand the resolver a
    // tombstone whose `client_id` is empty (forcing the DCR re-register
    // path, which already preserves `resource_*` via the record it carries
    // forward) but whose resource pair is intact.
    //
    // Only DCR-provenanced records (`registered_via_dcr == true`) participate
    // in the issuer-mismatch invalidation. Manually-supplied credentials and
    // legacy files (which deserialize with `registered_via_dcr = false`) are
    // preserved unconditionally — auto-discarding them and then silently
    // re-registering would break the "manual credentials survive" promise.
    let persisted_dcr = match persisted_dcr {
        Some(creds)
            if creds.registered_via_dcr
                && !dcr_issuer_allows_reuse(creds.issuer.as_deref(), issuer.as_deref()) =>
        {
            info!(
                endpoint = %name,
                stored_issuer = ?creds.issuer,
                current_issuer = ?issuer,
                "DCR credential issuer changed; invalidating requesting pair (resource pair preserved) and re-registering"
            );
            Some(DcrCredentials {
                client_id: String::new(),
                client_secret: None,
                client_secret_expires_at: 0,
                registered_at: creds.registered_at,
                issuer: None,
                resource_client_id: creds.resource_client_id,
                resource_client_secret: creds.resource_client_secret,
                registered_via_dcr: true,
            })
        }
        other => other,
    };

    // A DCR-provenanced record (`registered_via_dcr == true`) with a live
    // `registration_endpoint` always takes the interactive re-registration
    // heal path — including when `config.toml` still carries a stale DCR
    // `client_id` from the initial setup commit (setup stamps the non-secret
    // `client_id` into TOML — the secret lives only in the `.dcr.json` store,
    // though legacy configs may still carry the full pair — so
    // `config_client_id` is `Some` for every setup-created endpoint and would
    // otherwise skip this branch, breaking the RFC 7591 re-registration
    // promise for the most common shape of DCR endpoint). The DCR file is the authoritative
    // source for DCR-provenanced credentials — `watcher::resolve_oauth_client_creds`
    // already prefers it at startup — so we do not need to rewrite `config.toml`
    // here for the running adapter (Finding 2's `set_client_credentials`
    // override propagates the newly minted pair after the callback succeeds,
    // and the DCR file is what a restart consults). See spec: Root cause
    // analysis / Approach Part 2.
    let dcr_reregister = matches!(
        (persisted_dcr.as_ref(), registration_endpoint.as_ref()),
        (Some(creds), Some(_)) if creds.registered_via_dcr
    );

    let (client_id, client_secret, dcr_used) = if dcr_reregister {
        // Safe: the `matches!` above proved both are `Some`.
        let creds = persisted_dcr.as_ref().unwrap();
        let reg_endpoint = registration_endpoint.as_ref().unwrap();
        match dcr::register_client(reg_endpoint, &redirect_uri, &name, allow_insecure_oauth).await {
            Ok(resp) => {
                if let Some(ref tm) = state.token_manager {
                    // Compare-and-update via the atomic helper: only replace
                    // the on-disk record when it still has the DCR provenance
                    // AND its `client_id` matches the snapshot we resolved
                    // before calling `register_client`. If a concurrent
                    // `POST /credentials` landed between our snapshot and
                    // this save (rotating to manual creds or replacing
                    // resource_*), the compare fails and we leave the newer
                    // operator-managed record on disk instead of clobbering
                    // it. Round-4 finding R4-4.
                    //
                    // On compare failure we ALSO refuse to continue the
                    // auth-start with the unpersisted fresh DCR pair
                    // (round-5 finding R5-2): otherwise a successful
                    // callback would install `set_client_credentials`
                    // on the running adapter and save a token set bound
                    // to a `client_id` that never made it to disk,
                    // bypassing the R5-3 callback guard when the
                    // concurrent write was a manual rotation
                    // (`registered_via_dcr = false`). `snapshot_matched`
                    // uses `AtomicBool` (not `Cell`) so the future
                    // produced by this async fn stays `Send` for axum's
                    // `Handler` bound.
                    //
                    // `expected_client_id` is the ORIGINAL on-disk id
                    // captured at load time — not `creds.client_id`, which
                    // the R4-1 issuer-mismatch branch may have rewritten
                    // to `""` in memory (the file itself still holds the
                    // pre-invalidation id until we save).
                    let expected_client_id = persisted_dcr_disk_client_id.clone();
                    let new_client_id = resp.client_id.clone();
                    let new_client_secret = resp.client_secret.clone();
                    let expires_at = resp.client_secret_expires_at;
                    let bind_issuer = issuer.clone();
                    // `AtomicBool` (not `Cell`) so the future produced by
                    // this async fn stays `Send` for axum's `Handler`
                    // bound.
                    let snapshot_matched = std::sync::atomic::AtomicBool::new(true);
                    let update_res: Result<Option<DcrCredentials>, TokenError> = tm
                        .update_dcr(&name, |current| {
                            let matches_snapshot = match (current.as_ref(), expected_client_id.as_deref()) {
                                (Some(c), Some(expected)) => {
                                    c.registered_via_dcr && c.client_id == expected
                                }
                                (None, None) => true,
                                _ => false,
                            };
                            if !matches_snapshot {
                                snapshot_matched.store(false, std::sync::atomic::Ordering::SeqCst);
                                info!(
                                    endpoint = %name,
                                    expected_client_id = expected_client_id.as_deref().unwrap_or("<absent>"),
                                    current_client_id = current
                                        .as_ref()
                                        .map(|c| c.client_id.as_str())
                                        .unwrap_or("<absent>"),
                                    current_registered_via_dcr = current
                                        .as_ref()
                                        .map(|c| c.registered_via_dcr)
                                        .unwrap_or(false),
                                    "Skipping fresh-DCR save: on-disk record diverged from snapshot (concurrent operator write)"
                                );
                                return Ok(current);
                            }
                            let (resource_client_id, resource_client_secret) = current
                                .map(|c| (c.resource_client_id, c.resource_client_secret))
                                .unwrap_or_default();
                            Ok(Some(DcrCredentials {
                                client_id: new_client_id.clone(),
                                client_secret: new_client_secret.clone(),
                                client_secret_expires_at: expires_at,
                                registered_at: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs(),
                                issuer: bind_issuer.clone(),
                                resource_client_id,
                                resource_client_secret,
                                registered_via_dcr: true,
                            }))
                        })
                        .await;
                    if let Err(e) = update_res {
                        warn!(
                            endpoint = %name,
                            error = %e,
                            "Failed to persist re-registered DCR credentials"
                        );
                    }
                    if !snapshot_matched.load(std::sync::atomic::Ordering::SeqCst) {
                        warn!(
                            endpoint = %name,
                            "Auth-start superseded by concurrent credential rotation; refusing to continue with unpersisted fresh DCR pair (R5-2)"
                        );
                        return error_response(
                            StatusCode::CONFLICT,
                            "auth_start_superseded",
                            Some(
                                "Endpoint credentials were rotated by a concurrent operation \
                                 during this Authorize. Please click Authorize again.",
                            ),
                        )
                        .into_response();
                    }
                }
                if config_client_id
                    .as_ref()
                    .is_some_and(|cid| cid != &resp.client_id)
                {
                    info!(
                        endpoint = %name,
                        stale_config_client_id = ?config_client_id,
                        fresh_dcr_client_id = %resp.client_id,
                        "DCR re-registration minted a new client_id; config.toml still \
                         carries the previous id — the running adapter's in-memory \
                         override and the DCR file are the authoritative sources \
                         (resolve_oauth_client_creds prefers the DCR file on restart)"
                    );
                }
                (resp.client_id, resp.client_secret, true)
            }
            Err(e) => {
                if creds.client_id.is_empty() {
                    // Post-self-heal tombstone: the stored requesting
                    // pair was cleared by a prior `invalid_client`
                    // self-heal and there is nothing to fall back to.
                    // Producing an authorize URL with `client_id=`
                    // cannot succeed; surface a clear error so the
                    // caller can retry once the registration endpoint
                    // is reachable again.
                    warn!(
                        endpoint = %name,
                        error = %e,
                        "DCR re-registration failed and stored requesting client is a self-heal tombstone (no fallback)"
                    );
                    return error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "dcr_registration_unavailable",
                        Some(&format!(
                            "DCR re-registration failed and no viable stored client_id remains. \
                             Retry once the registration endpoint is reachable. Details: {e}"
                        )),
                    )
                    .into_response();
                }
                warn!(
                    endpoint = %name,
                    error = %e,
                    "DCR re-registration failed; falling back to stored credentials"
                );
                (creds.client_id.clone(), creds.client_secret.clone(), true)
            }
        }
    } else if let Some(cid) = config_client_id {
        // TOML has a client_id and the DCR record (if any) is either
        // manually supplied (`registered_via_dcr == false`) or the
        // authorization server does not expose a registration endpoint —
        // in both cases we reuse the record as-is. Prefer the
        // DCR-persisted `client_secret` when the DCR `client_id` matches;
        // otherwise fall back to whatever is in TOML (which may be `None`
        // for endpoints added via the desktop UI).
        match persisted_dcr {
            Some(creds) if creds.client_id == cid => (cid, creds.client_secret, false),
            Some(creds) => {
                warn!(
                    endpoint = %name,
                    dcr_client_id = %creds.client_id,
                    config_client_id = %cid,
                    "DCR client_id does not match config client_id; using config credentials"
                );
                (cid, config_client_secret, false)
            }
            None => (cid, config_client_secret, false),
        }
    } else if let Some(creds) = persisted_dcr {
        // No config `client_id`, and either the record is
        // manually-supplied (`registered_via_dcr == false`) or there is
        // no live registration endpoint to re-register against. Reuse
        // the stored record unchanged.
        (creds.client_id, creds.client_secret, true)
    } else if let Some(ref reg_endpoint) = registration_endpoint {
        // Attempt dynamic client registration (URL guard + pinned client inside)
        match dcr::register_client(reg_endpoint, &redirect_uri, &name, allow_insecure_oauth).await {
            Ok(resp) => {
                // Persist the new credentials
                if let Some(ref tm) = state.token_manager {
                    let creds = DcrCredentials {
                        client_id: resp.client_id.clone(),
                        client_secret: resp.client_secret.clone(),
                        client_secret_expires_at: resp.client_secret_expires_at,
                        registered_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        issuer: issuer.clone(),
                        registered_via_dcr: true,
                        ..Default::default()
                    };
                    if let Err(e) = tm.save_dcr(&name, &creds).await {
                        warn!(endpoint = %name, error = %e, "Failed to persist DCR credentials");
                    }
                }
                (resp.client_id, resp.client_secret, true)
            }
            Err(e) => {
                warn!(endpoint = %name, error = %e, "DCR registration failed");
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(OAuthDcrUnsupportedResponse {
                        error: "dcr_unsupported".to_string(),
                        message: format!(
                            "Dynamic Client Registration failed: {e}. \
                             Submit credentials manually via POST /api/endpoints/{name}/oauth/credentials."
                        ),
                        authorization_endpoint: Some(authorization_endpoint.clone()),
                        token_endpoint: Some(token_endpoint.clone()),
                        scopes_supported: discovered_scopes.clone(),
                    }),
                )
                    .into_response();
            }
        }
    } else {
        // No registration endpoint — DCR not available
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(OAuthDcrUnsupportedResponse {
                error: "dcr_unsupported".to_string(),
                message: format!(
                    "No client_id configured and server does not support Dynamic Client Registration. \
                     Submit credentials manually via POST /api/endpoints/{name}/oauth/credentials."
                ),
                authorization_endpoint: Some(authorization_endpoint.clone()),
                token_endpoint: Some(token_endpoint.clone()),
                scopes_supported: discovered_scopes.clone(),
            }),
        )
            .into_response();
    };

    // ── Step 3: Build PKCE + register flow ─────────────────────────────
    let pkce = PkceChallenge::generate();
    let code_challenge = pkce.code_challenge.clone();

    let Some(state_param) = flow_mgr
        .start_flow_if_current(
            start_generation,
            &name,
            &token_endpoint,
            &client_id,
            client_secret.as_deref(),
            pkce,
            &redirect_uri,
            issuer.as_deref(),
            iss_supported,
        )
        .await
    else {
        // A reset landed while this start was mid-discovery: this start
        // predates the reset, so its URL must not be handed out (it lacks
        // the reset's forced consent). The reset's own follow-up start is
        // the authoritative one.
        return error_response(
            StatusCode::CONFLICT,
            "superseded_by_reset",
            Some("Authorization was reset while this start was in progress; use the reset's authorize URL or retry"),
        )
        .into_response();
    };

    // ── Step 4: Build authorization URL ────────────────────────────────
    let mut authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorization_endpoint,
        urlencoding(&client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state_param),
        urlencoding(&code_challenge),
    );

    // Forced consent ("Reset authorization"): `prompt=consent` makes OIDC
    // providers re-show the consent screen instead of silently reusing the
    // prior grant. Non-OIDC providers ignore unknown parameters, so this is
    // safe to append unconditionally when requested.
    if force_consent {
        authorize_url.push_str("&prompt=consent");
    }

    // Google needs `access_type=offline` for a refresh token to be issued
    // (shared helper — see `crate::oauth::append_google_authorize_params`).
    append_google_authorize_params(&mut authorize_url, &authorization_endpoint);

    // Scope accumulation for step-up authorization: union the scopes we'd
    // request today (config scopes) with any previously-granted scopes from a
    // persisted TokenSet, so re-authorizing for more access never drops scopes
    // the user already granted. First login (no prior token) is unchanged.
    let effective_scopes = scopes.unwrap_or_default();
    let requested_scope = effective_scopes.join(" ");
    let prior_scope = if let Some(ref tm) = state.token_manager {
        tm.load(&name).await.ok().flatten().and_then(|t| t.scope)
    } else {
        None
    };
    let merged_scope = merge_scopes(prior_scope.as_deref(), &requested_scope);
    if !merged_scope.is_empty() {
        authorize_url.push_str(&format!("&scope={}", urlencoding(&merged_scope)));
    }

    let discovery_info = auth_server_label.map(|auth_server| OAuthDiscoveryInfo {
        auth_server,
        dcr_used,
        scopes_available: discovered_scopes,
    });

    Json(OAuthStartResponse {
        authorize_url,
        discovery: discovery_info,
    })
    .into_response()
}

/// POST /api/endpoints/:name/oauth/credentials
///
/// Accept manually provided client credentials (DCR fallback).
/// Persists them via TokenManager so the next `/oauth/start` can use them.
async fn oauth_credentials(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
    Json(body): Json<OAuthCredentialsRequest>,
) -> impl IntoResponse {
    // Verify endpoint exists
    {
        let config = state.config.read().await;
        let exists = config.endpoints.iter().any(|e| e.name == name);
        if !exists {
            return endpoint_not_found(&name).into_response();
        }
    }

    if body.client_id.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "client_id must not be empty", None)
            .into_response();
    }

    let Some(ref tm) = state.token_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Token manager not available",
            None,
        )
        .into_response();
    };

    let creds = DcrCredentials {
        client_id: body.client_id,
        client_secret: body.client_secret,
        client_secret_expires_at: 0,
        registered_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        // Manually-supplied credentials are user-managed, not bound to a
        // discovered issuer; leave None so they are always reused as-is.
        issuer: None,
        // User-supplied via /oauth/credentials → never auto-discard.
        registered_via_dcr: false,
        ..Default::default()
    };

    if let Err(e) = tm.save_dcr(&name, &creds).await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save credentials",
            Some(&e.to_string()),
        )
        .into_response();
    }

    Json(serde_json::json!({ "status": "saved" })).into_response()
}

/// POST /api/endpoints/:name/credentials
///
/// Persist caller-supplied credentials for an endpoint via `TokenManager` (the
/// `{name}.dcr.json` DCR file, 0600). Never writes them to `config.toml`.
///
/// Two independently-optional credential groups are merged into the single
/// per-endpoint DCR record so updating one never wipes the other:
///   * the requesting OAuth `client_id`/`client_secret` (Wave 3a) used by the
///     endpoint's own OAuth flow;
///   * the optional EMA **resource** `client_id`/`client_secret` pair (R3),
///     presented only at the MAS in Step 3. R3 re-scoped this pair from the org
///     record to the endpoint because it is per-resource.
///
/// Merge semantics per field: absent preserves the stored value, an empty
/// string clears it, a non-empty value sets it. A requesting `client_secret`
/// requires a `client_id` (existing or supplied) — a secret with no client_id
/// is rejected with 400. When the merged record holds no credential material
/// the DCR file is deleted.
async fn set_endpoint_credentials(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
    Json(body): Json<EndpointCredentialsRequest>,
) -> impl IntoResponse {
    // Verify endpoint exists.
    {
        let config = state.config.read().await;
        let exists = config.endpoints.iter().any(|e| e.name == name);
        if !exists {
            return endpoint_not_found(&name).into_response();
        }
    }

    let Some(ref tm) = state.token_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Token manager not available",
            None,
        )
        .into_response();
    };

    // The requesting_touched flag depends only on the request body, so
    // compute it once and reuse it inside the closure below.
    let requesting_touched = body.client_id.is_some() || body.client_secret.is_some();

    // Read-modify-write the DCR record under `dcr_write_lock` so that a
    // concurrent `invalid_client` self-heal
    // (`TokenManager::clear_dcr_requesting_client`) cannot land between
    // our load and save and be silently clobbered by the merged record
    // computed from the pre-heal snapshot.
    let outcome = tm
        .update_dcr(
            &name,
            |existing| -> Result<Option<DcrCredentials>, SetCredsError> {
                // keep (absent) / clear (empty) / set (non-empty) per field.
                let merge = |action: Option<&str>, current: Option<String>| match action {
                    None => current,
                    Some("") => None,
                    Some(s) => Some(s.to_string()),
                };

                // client_id is a plain (non-optional) field on the record: absent
                // keeps the stored id (empty for a resource-only EMA endpoint), a
                // value replaces it.
                let merged_client_id = match body.client_id.as_deref().map(str::trim) {
                    None => existing
                        .as_ref()
                        .map(|c| c.client_id.clone())
                        .unwrap_or_default(),
                    Some(s) => s.to_string(),
                };
                let client_secret = merge(
                    body.client_secret.as_deref().map(str::trim),
                    existing.as_ref().and_then(|c| c.client_secret.clone()),
                );
                let resource_client_id = merge(
                    body.resource_client_id.as_deref().map(str::trim),
                    existing.as_ref().and_then(|c| c.resource_client_id.clone()),
                );
                let resource_client_secret = merge(
                    body.resource_client_secret.as_deref().map(str::trim),
                    existing
                        .as_ref()
                        .and_then(|c| c.resource_client_secret.clone()),
                );

                // A requesting secret has no meaning without a client_id to pair it with.
                if client_secret.is_some() && merged_client_id.is_empty() {
                    return Err(SetCredsError::Validation(
                        "client_id must not be empty when setting client_secret",
                    ));
                }

                // Symmetric guard: a resource secret must be paired with a resource client_id.
                if resource_client_secret.is_some()
                    && resource_client_id
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or("")
                        .is_empty()
                {
                    return Err(SetCredsError::Validation(
                        "resource_client_id must not be empty when setting resource_client_secret",
                    ));
                }

                // Nothing left to persist → remove the DCR record entirely.
                if merged_client_id.is_empty()
                    && client_secret.is_none()
                    && resource_client_id.is_none()
                    && resource_client_secret.is_none()
                {
                    return Ok(None);
                }

                // If the caller touched the requesting client_id/secret at all
                // (set OR clear), the requesting creds are now user-managed →
                // force the DCR provenance flag off AND clear any previous
                // `issuer` binding. The provenance flag alone is not enough:
                // `oauth_start` rejects a credential whose stored `issuer`
                // differs from the currently discovered one BEFORE it checks
                // the provenance flag, so a manual replace that left the
                // DCR-era `issuer` in place would be discarded on a later
                // issuer change and silently overwritten by a fresh DCR
                // registration — exactly the "manual credentials survive"
                // promise this endpoint makes to callers. Updates that only
                // touch the `resource_*` pair leave the requesting creds
                // untouched, so preserve the existing provenance flag AND
                // `issuer`.
                let registered_via_dcr = if requesting_touched {
                    false
                } else {
                    existing
                        .as_ref()
                        .map(|c| c.registered_via_dcr)
                        .unwrap_or(false)
                };
                let issuer = if requesting_touched {
                    None
                } else {
                    existing.as_ref().and_then(|c| c.issuer.clone())
                };

                Ok(Some(DcrCredentials {
                    client_id: merged_client_id,
                    client_secret,
                    client_secret_expires_at: existing
                        .as_ref()
                        .map(|c| c.client_secret_expires_at)
                        .unwrap_or(0),
                    registered_at: existing.as_ref().map(|c| c.registered_at).unwrap_or_else(
                        || {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                        },
                    ),
                    // Manually-supplied credentials are user-managed and not
                    // bound to a discovered issuer; preserve any existing
                    // binding only when the requesting pair was not touched,
                    // and never invent one.
                    issuer,
                    resource_client_id,
                    resource_client_secret,
                    registered_via_dcr,
                }))
            },
        )
        .await;

    let saved = match outcome {
        Ok(saved) => saved,
        Err(SetCredsError::Validation(msg)) => {
            return error_response(StatusCode::BAD_REQUEST, msg, None).into_response();
        }
        Err(SetCredsError::Storage(e)) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to persist credentials",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };

    let client_secret_set = saved
        .as_ref()
        .and_then(|c| c.client_secret.as_ref())
        .is_some();
    let resource_client_secret_set = saved
        .as_ref()
        .and_then(|c| c.resource_client_secret.as_ref())
        .is_some();

    Json(serde_json::json!({
        "ok": true,
        "client_secret_set": client_secret_set,
        "resource_client_secret_set": resource_client_secret_set,
    }))
    .into_response()
}

/// Error surface for the `set_endpoint_credentials` update closure. Split
/// so the caller can render a `400 Bad Request` for validation failures
/// (with the exact message) and a `500 Internal Server Error` for storage
/// failures, while `From<TokenError>` lets `TokenManager::update_dcr`
/// propagate IO/serde errors transparently.
enum SetCredsError {
    Validation(&'static str),
    Storage(TokenError),
}

impl From<TokenError> for SetCredsError {
    fn from(e: TokenError) -> Self {
        SetCredsError::Storage(e)
    }
}

/// GET /api/endpoints/:name/credentials
///
/// Returns the resolved credential view for an endpoint, preferring the DCR
/// file and falling back to `EndpointConfig` (legacy TOML). Never returns the
/// secret value itself — only `client_secret_set: bool`.
async fn get_endpoint_credentials(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Verify endpoint exists and capture legacy fields.
    let (cfg_client_id, cfg_secret, cfg_oauth_server_url) = {
        let config = state.config.read().await;
        let Some(ep) = config.endpoints.iter().find(|e| e.name == name) else {
            return endpoint_not_found(&name).into_response();
        };
        (
            ep.client_id.clone(),
            ep.client_secret.clone(),
            ep.oauth_server_url.clone(),
        )
    };

    if let Some(ref tm) = state.token_manager {
        match tm.load_dcr(&name).await {
            Ok(Some(creds)) => {
                let resource_client_secret_set = creds.resource_client_secret.is_some();
                return Json(EndpointCredentialsResponse {
                    client_id: Some(creds.client_id).filter(|s| !s.is_empty()),
                    client_secret_set: creds.client_secret.is_some(),
                    oauth_server_url: cfg_oauth_server_url,
                    resource_client_id: creds.resource_client_id,
                    resource_client_secret_set,
                    source: "dcr",
                })
                .into_response();
            }
            Ok(None) => {}
            Err(e) => {
                warn!(endpoint = %name, error = %e, "Failed to read DCR credentials; falling back to config");
            }
        }
    }

    let source = if cfg_client_id.is_some() || cfg_secret.is_some() {
        "config"
    } else {
        "none"
    };
    Json(EndpointCredentialsResponse {
        client_id: cfg_client_id,
        client_secret_set: cfg_secret
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        oauth_server_url: cfg_oauth_server_url,
        resource_client_id: None,
        resource_client_secret_set: false,
        source,
    })
    .into_response()
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
            // Only meaningful when a refresh token exists
            let next_refresh_at = if !has_refresh_token {
                None
            } else {
                tokens
                    .as_ref()
                    .and_then(|t| match (t.issued_at, t.expires_at) {
                        (Some(issued), Some(expires)) if expires > issued => {
                            let lifetime = expires - issued;
                            let seventy_five_pct = issued + (lifetime * 3 / 4);
                            let five_min_before = expires.saturating_sub(300);
                            Some(std::cmp::min(seventy_five_pct, five_min_before))
                        }
                        _ => None,
                    })
            };

            let status = match oauth_state {
                OAuthState::Authenticated => "authenticated",
                OAuthState::NeedsLogin => "needs_login",
                OAuthState::Refreshing => "refreshing",
                OAuthState::AuthRequired => "auth_required",
                OAuthState::Disconnected => "disconnected",
                OAuthState::ConnectionFailed => "connection_failed",
            };

            let state_str = format!("{:?}", oauth_state);

            let history = inner.transition_history.read().await;
            let transition_history: Vec<TransitionHistoryEntry> = history
                .iter()
                .map(|r| TransitionHistoryEntry {
                    from: format!("{:?}", r.from),
                    to: format!("{:?}", r.to),
                    reason: r.reason.clone(),
                    ago_ms: r.timestamp.elapsed().as_millis() as u64,
                })
                .collect();

            return Json(OAuthStatusDetailedResponse {
                status: status.to_string(),
                has_access_token,
                has_refresh_token,
                expires_at,
                expires_in_seconds,
                last_refreshed_at,
                next_refresh_at,
                state: state_str,
                transition_history,
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
    if let Err(e) = inner.disconnect().await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to delete tokens from disk",
            Some(&format!(
                "The grant would survive a relay restart; retry the revoke. ({})",
                e
            )),
        )
        .into_response();
    }

    Json(OAuthRevokeResponse {
        status: "disconnected".to_string(),
        endpoint: name,
    })
    .into_response()
}

/// POST /api/endpoints/:name/oauth/reset
///
/// "Reset authorization": discard the old grant and force a fresh consent
/// screen. Sequencing: disconnect the OAuth adapter first (local token
/// deletion via the same `disconnect()` path as `/oauth/revoke` — upstream
/// RFC 7009 revocation rides along once `disconnect()` gains it), then start
/// a new authorization flow with `force_consent`, returning the composed
/// `authorize_url`. Unlike `/oauth/revoke`, the client registration (DCR
/// record) is preserved across the reset: only the grant is discarded, so
/// the follow-up start flow can reuse the registered client instead of
/// losing a secret that exists nowhere else.
async fn oauth_reset(
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

    let inner = {
        let inners_guard = inners.read().await;
        match inners_guard.get(&name) {
            Some(inner) => inner.clone(),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "endpoint is not configured for OAuth",
                    Some(&format!("Endpoint '{}' is not an OAuth endpoint", name)),
                )
                .into_response();
            }
        }
    };

    // Serialize the whole discard (generation bump + disconnect + DCR
    // restore) against the callback's commit phase: the callback holds this
    // same per-endpoint lock from its post-exchange generation check through
    // its token save / adapter apply, so a callback mid-commit either
    // finishes before we bump (and its tokens are wiped by the disconnect
    // below) or checks the generation after our bump and refuses to commit.
    // Without it a callback that passed its check could persist the
    // pre-reset grant AFTER our disconnect, undoing the reset.
    let commit_guard = match state.oauth_flow_manager {
        Some(ref flow_mgr) => Some(flow_mgr.commit_lock(&name).await),
        None => None,
    };
    let _commit_guard = match commit_guard {
        Some(ref lock) => Some(lock.lock().await),
        None => None,
    };

    // Invalidate pending flows for this endpoint and bump its reset
    // generation BEFORE disconnecting: an authorize URL handed out by a
    // pre-reset `/oauth/start` stays valid for up to FLOW_MAX_AGE, and its
    // late callback would otherwise complete after the disconnect and
    // clobber the reset with the pre-reset grant. The generation bump also
    // covers callbacks already consumed and mid token exchange — the
    // callback handler refuses to commit a flow from a stale generation.
    if let Some(ref flow_mgr) = state.oauth_flow_manager {
        let removed = flow_mgr.invalidate_endpoint(&name).await;
        if removed > 0 {
            info!(endpoint = %name, removed, "Reset: invalidated pending OAuth flows");
        }
    }

    // Snapshot the client registration before disconnect: `disconnect()`
    // deletes the DCR record along with the tokens, but reset must only
    // discard the grant. Desktop-created DCR endpoints keep the
    // client_secret solely in that record, and manually supplied
    // credentials live only there — losing it would break the one-step
    // reset (no secret / `dcr_unsupported` on providers without DCR).
    let preserved_dcr = match state.token_manager {
        Some(ref tm) => tm.load_dcr(&name).await.ok().flatten(),
        None => None,
    };

    // Discard the old grant BEFORE composing the new authorize URL, so the
    // start flow sees a clean slate. A failed local deletion fails the
    // whole reset: reporting success while the old token file survives on
    // disk would silently restore the discarded grant on the next restart.
    if let Err(e) = inner.disconnect().await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to delete tokens from disk",
            Some(&format!(
                "Reset aborted: the old grant would survive a relay restart; retry the reset. ({})",
                e
            )),
        )
        .into_response();
    }

    // Restore the client registration so the start flow reuses it.
    if let (Some(tm), Some(creds)) = (state.token_manager.as_ref(), preserved_dcr.as_ref()) {
        if let Err(e) = tm.save_dcr(&name, creds).await {
            warn!(endpoint = %name, error = %e, "Reset: failed to restore client registration after disconnect");
        }
    }

    // Release before starting the replacement flow: `oauth_start_inner`
    // performs discovery (network) and must not hold the commit lock.
    drop(_commit_guard);
    drop(commit_guard);

    oauth_start_inner(state, name, true).await
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
    let refresh_epoch = inner.current_grant_epoch();
    match inner.do_token_refresh_with_epoch(refresh_epoch).await {
        Ok(new_tokens) => {
            let expires_at = new_tokens.expires_at;
            let refreshed_at = new_tokens.issued_at;
            let outcome = inner
                .apply_refreshed_tokens(new_tokens, refresh_epoch)
                .await;

            // A successful token-endpoint response is not a committed
            // refresh: the commit is dropped when the grant was discarded
            // or replaced (disconnect/reset/re-login) while the refresh was
            // in flight. Reporting 200 with the discarded set's metadata
            // would falsely describe tokens that were never installed.
            if outcome == RefreshCommitOutcome::DroppedStaleGrant {
                return error_response(
                    StatusCode::CONFLICT,
                    "token refresh not committed",
                    Some("the grant was discarded or replaced (disconnect/reset/re-login) while the refresh was in flight"),
                )
                .into_response();
            }

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
        // Same concurrent disconnect/replacement, caught before the token
        // POST: not an upstream failure, so 409 rather than 502.
        Err(e @ crate::oauth::OAuthError::StaleGrant { .. }) => error_response(
            StatusCode::CONFLICT,
            "token refresh not committed",
            Some(&e.to_string()),
        )
        .into_response(),
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            "token refresh failed",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

/// GET /api/endpoints/:name/oauth/metrics
///
/// Returns in-process metric counters for the OAuth adapter (JSON).
async fn oauth_metrics(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
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

    Json(inner.metrics.snapshot()).into_response()
}

// ---------------------------------------------------------------------------
// OAuth capability probe (add-time)
// ---------------------------------------------------------------------------

/// Request body for POST /api/oauth/probe
#[derive(Deserialize)]
struct OAuthProbeRequest {
    /// The MCP server URL to probe for OAuth support.
    url: String,
}

/// Response for POST /api/oauth/probe.
///
/// JSON contract (consumed by the desktop add-server flow):
///   Request:  `{ "url": "<mcp server url>" }`
///   Response: `{ "oauth_supported": bool,
///               "authorization_server"?: string,
///               "scopes_supported"?: [string] }`
///
/// `authorization_server` and `scopes_supported` are present only when
/// `oauth_supported` is `true` (and `scopes_supported` only when non-empty).
#[derive(Serialize)]
struct OAuthProbeResponse {
    oauth_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scopes_supported: Option<Vec<String>>,
}

/// POST /api/oauth/probe
///
/// Best-effort check of whether an MCP server supports OAuth (RFC 9728 →
/// RFC 8414), used by the desktop add-server flow to decide whether to start
/// an OAuth setup or proceed with a plain HTTP add. This runs discovery only —
/// it does NOT start a flow, perform DCR, or persist anything.
///
/// JSON contract:
///   Request:  `{ "url": "<mcp server url>" }`
///   Response: `{ "oauth_supported": bool,
///               "authorization_server"?: string,
///               "scopes_supported"?: [string] }`
///
/// Any discovery failure (no metadata, 404, connection error, timeout) is
/// reported as a normal HTTP 200 with `oauth_supported: false` so the caller
/// can fall back to a plain (non-OAuth) add without treating it as an error.
async fn oauth_probe(
    State(state): State<ManagementState>,
    Json(body): Json<OAuthProbeRequest>,
) -> impl IntoResponse {
    use crate::oauth::discovery;

    let allow_insecure_oauth = {
        let config = state.config.read().await;
        config.relay.allow_insecure_oauth.unwrap_or(false)
    };

    // Bound the overall probe so a slow/unreachable well-known endpoint can
    // never stall the caller. Discovery performs up to two sequential fetches
    // (each with its own 10s timeout); cap the whole operation here as a
    // backstop.
    let probe = discovery::discover_oauth_server(body.url.trim(), allow_insecure_oauth);
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), probe).await;

    match result {
        Ok(Ok(disc)) => Json(OAuthProbeResponse {
            oauth_supported: true,
            authorization_server: Some(disc.auth_server_url),
            scopes_supported: if disc.scopes_supported.is_empty() {
                None
            } else {
                Some(disc.scopes_supported)
            },
        })
        .into_response(),
        Ok(Err(_)) | Err(_) => Json(OAuthProbeResponse {
            oauth_supported: false,
            authorization_server: None,
            scopes_supported: None,
        })
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// EMA capability probe (per-organization "which MCP servers can I reach?")
// ---------------------------------------------------------------------------

/// Concurrency bound for per-resource probes within a single batch.
const ORG_PROBE_CONCURRENCY: usize = 8;

/// Per-probe wall-clock cap (discovery + token exchange). Bounds a slow or
/// unreachable resource so it can never stall the whole batch.
const ORG_PROBE_TIMEOUT_SECS: u64 = 15;

/// TTL for cached probe outcomes keyed by `(org, resource)`. A cache hit within
/// this window skips re-running discovery + the ID-JAG exchange.
const ORG_PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Process-global short-TTL cache of probe outcomes keyed by `(org, resource)`.
/// A static keeps `ManagementState` (and its many constructors) untouched; the
/// TTL bounds staleness and entries are uniquely keyed by the desktop-supplied
/// resource URL.
static ORG_PROBE_CACHE: std::sync::LazyLock<
    std::sync::RwLock<HashMap<(String, String), OrgProbeCacheEntry>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(HashMap::new()));

/// One cached probe outcome plus its insertion time (for TTL expiry).
struct OrgProbeCacheEntry {
    status: OrgProbeStatus,
    server_as_issuer: Option<String>,
    inserted: Instant,
}

/// Reachability of a single MCP resource for the requesting organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum OrgProbeStatus {
    /// The IdP minted an ID-JAG for this resource (the ID-JAG is discarded —
    /// the probe persists nothing).
    Accessible,
    /// The IdP refused the exchange with a terminal authorization denial.
    Denied,
    /// Discovery failed, the exchange hit a transport/expiry error, or the
    /// per-probe timeout elapsed.
    Unreachable,
}

/// Request body for `POST /api/organizations/{org}/probe`.
#[derive(Deserialize)]
struct OrgProbeRequest {
    /// Desktop-supplied candidate MCP server URLs. The relay stays
    /// catalog-agnostic and only probes exactly these candidates.
    #[serde(default)]
    resources: Vec<String>,
}

/// One probe outcome for a single resource.
#[derive(Serialize)]
struct OrgProbeResult {
    resource: String,
    status: OrgProbeStatus,
    /// The resource AS issuer (RFC 8414 `issuer`) discovered for this resource,
    /// present whenever discovery succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    server_as_issuer: Option<String>,
}

/// Response body for `POST /api/organizations/{org}/probe`.
#[derive(Serialize)]
struct OrgProbeResponse {
    results: Vec<OrgProbeResult>,
}

/// Run the discovery → ID-JAG-exchange chain for a single resource and map the
/// outcome to an [`OrgProbeStatus`]. Persists nothing: a successful exchange's
/// ID-JAG is discarded and Step 3 (redemption) is never run.
#[allow(clippy::too_many_arguments)]
async fn run_org_probe_chain(
    resource: &str,
    idp_token_endpoint: &str,
    id_token: &str,
    allow_insecure: bool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> (OrgProbeStatus, Option<String>) {
    let resource = resource.trim();
    let chain = async {
        let disc =
            match crate::oauth::discovery::discover_oauth_server(resource, allow_insecure).await {
                Ok(d) => d,
                // Discovery failed (no metadata, 404, transport, SSRF guard).
                Err(_) => return (OrgProbeStatus::Unreachable, None),
            };
        let as_issuer = disc.issuer;
        match crate::oauth::ema::exchange_for_id_jag(
            idp_token_endpoint,
            id_token,
            &as_issuer,
            resource,
            // Connectivity probe only: no resource scope is threaded (R2).
            None,
            allow_insecure,
            client_id,
            client_secret,
        )
        .await
        {
            // Accessible: discard the ID-JAG, persist nothing.
            Ok(_id_jag) => (OrgProbeStatus::Accessible, Some(as_issuer)),
            Err(crate::oauth::ema::EmaError::AuthorizationDenied { .. }) => {
                (OrgProbeStatus::Denied, Some(as_issuer))
            }
            // Transport/expiry/validation errors are treated as unreachable; the
            // AS issuer is still known since discovery succeeded.
            Err(_) => (OrgProbeStatus::Unreachable, Some(as_issuer)),
        }
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(ORG_PROBE_TIMEOUT_SECS),
        chain,
    )
    .await
    {
        Ok(outcome) => outcome,
        // Per-probe timeout elapsed.
        Err(_) => (OrgProbeStatus::Unreachable, None),
    }
}

/// Probe one resource, consulting (and populating) the short-TTL cache.
#[allow(clippy::too_many_arguments)]
async fn probe_one_resource(
    org: String,
    resource: String,
    idp_token_endpoint: String,
    id_token: String,
    allow_insecure: bool,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> OrgProbeResult {
    let key = (org, resource.clone());

    // Cache hit within TTL: skip re-discovery + re-exchange.
    if let Ok(cache) = ORG_PROBE_CACHE.read() {
        if let Some(entry) = cache.get(&key) {
            if entry.inserted.elapsed() < ORG_PROBE_CACHE_TTL {
                return OrgProbeResult {
                    resource,
                    status: entry.status,
                    server_as_issuer: entry.server_as_issuer.clone(),
                };
            }
        }
    }

    let (status, server_as_issuer) = run_org_probe_chain(
        &resource,
        &idp_token_endpoint,
        &id_token,
        allow_insecure,
        client_id.as_deref(),
        client_secret.as_deref(),
    )
    .await;

    if let Ok(mut cache) = ORG_PROBE_CACHE.write() {
        cache.insert(
            key,
            OrgProbeCacheEntry {
                status,
                server_as_issuer: server_as_issuer.clone(),
                inserted: Instant::now(),
            },
        );
    }

    OrgProbeResult {
        resource,
        status,
        server_as_issuer,
    }
}

/// POST /api/organizations/{org}/probe
///
/// The "Detecting available MCP servers…" engine. For each desktop-supplied
/// candidate `resource`, runs RFC 9728 → RFC 8414 discovery and an RFC 8693
/// ID-JAG token exchange against the org's IdP, mapping the outcome to
/// `accessible` / `denied` / `unreachable`. Probes run with bounded parallelism
/// and a per-probe timeout, results are cached briefly per `(org, resource)`,
/// and the probe persists **nothing** (the ID-JAG is discarded; no token files
/// are written).
///
/// JSON contract:
///   Request:  `{ "resources": ["<mcp url>", ...] }`
///   Response: `{ "results": [{ "resource", "status", "server_as_issuer"? }] }`
async fn probe_organization(
    State(state): State<ManagementState>,
    Path(org): Path<String>,
    Json(body): Json<OrgProbeRequest>,
) -> impl IntoResponse {
    // Resolve the org's IdP issuer (and the insecure-loopback policy) from config.
    let (idp_issuer, org_client_id, allow_insecure) = {
        let config = state.config.read().await;
        let Some(found) = config.organizations.iter().find(|o| o.name == org) else {
            return error_response(
                StatusCode::NOT_FOUND,
                "organization not found",
                Some(&format!("No organization named '{org}'.")),
            )
            .into_response();
        };
        (
            found.idp.clone(),
            found
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            config.relay.allow_insecure_oauth.unwrap_or(false),
        )
    };

    let Some(token_manager) = state.token_manager.clone() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "token manager not available",
            None,
        )
        .into_response();
    };

    // Org-keyed pooled IdP credentials (Wave 2). Absence ⇒ the org has not
    // completed IdP SSO, so no probe can run.
    let creds = match token_manager.load_idp(&org).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return error_response(
                StatusCode::CONFLICT,
                "organization not authenticated",
                Some(&format!(
                    "No IdP credentials stored for organization '{org}'. Authenticate it first."
                )),
            )
            .into_response();
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "credential store error",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };

    // Resolve the IdP token endpoint once for the whole batch (RFC 8414 / OIDC).
    let idp_token_endpoint =
        match crate::oauth::discovery::discover_authorization_server(&idp_issuer, allow_insecure)
            .await
        {
            Ok(disc) => disc.token_endpoint,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "idp discovery failed",
                    Some(&format!(
                        "Could not resolve IdP token endpoint for '{idp_issuer}': {e}"
                    )),
                )
                .into_response();
            }
        };

    let id_token = creds.id_token;

    // Confidential-client support: load the optional org `client_secret` from
    // the secure credential store (`{org}.dcr.json`) and thread it into the
    // probe's RFC 8693 Step 2 exchange so confidential clients authenticate
    // with `client_secret_post`. Only honour the stored secret when the DCR
    // `client_id` matches the resolved org `client_id`; otherwise treat the
    // secret as stale and fall back to the public flow. The probe never runs
    // Step 3 so no resource-AS leg is involved.
    let org_client_secret = match org_client_id.as_deref() {
        Some(cid) => match token_manager.load_dcr(&org).await {
            Ok(Some(dcr)) if dcr.client_id == cid => dcr.client_secret,
            Ok(_) => None,
            Err(e) => {
                warn!(organization = %org, error = %e, "Failed to load org DCR credentials; probing as public client");
                None
            }
        },
        None => None,
    };

    use futures_util::stream::StreamExt;
    let results: Vec<OrgProbeResult> =
        futures_util::stream::iter(body.resources.into_iter().map(|resource| {
            let org = org.clone();
            let idp_token_endpoint = idp_token_endpoint.clone();
            let id_token = id_token.clone();
            let client_id = org_client_id.clone();
            let client_secret = org_client_secret.clone();
            async move {
                probe_one_resource(
                    org,
                    resource,
                    idp_token_endpoint,
                    id_token,
                    allow_insecure,
                    client_id,
                    client_secret,
                )
                .await
            }
        }))
        .buffered(ORG_PROBE_CONCURRENCY)
        .collect()
        .await;

    Json(OrgProbeResponse { results }).into_response()
}

// ---------------------------------------------------------------------------
// OAuth setup (preflight) route handlers
// ---------------------------------------------------------------------------

/// Request body for POST /api/oauth/setup
#[derive(Deserialize)]
struct OAuthSetupRequest {
    name: String,
    url: String,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    tool_prefix: Option<String>,
    /// If provided, skip DCR and use these client credentials directly.
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    /// If provided, use this URL as the discovery base instead of `url`.
    #[serde(default)]
    oauth_server_url: Option<String>,
    /// Optional override for the advertised server type. Persisted into the
    /// resulting `[[endpoints]]` entry as `server_type_override`.
    #[serde(default)]
    server_type_override: Option<String>,
}

/// Response for POST /api/oauth/setup
#[derive(Serialize)]
struct OAuthSetupResponse {
    session_id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorize_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    discovery: Option<OAuthDiscoveryInfo>,
    /// Set when DCR fails and manual credentials are needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    dcr_error: Option<String>,
}

/// Response for GET /api/oauth/setup/:id/status
#[derive(Serialize)]
struct OAuthSetupStatusResponse {
    session_id: String,
    status: String,
    name: String,
    url: String,
}

/// POST /api/oauth/setup
///
/// Creates a transient setup session: discovers OAuth metadata, attempts DCR,
/// and returns the authorize URL — all without writing to config.toml.
async fn oauth_setup(
    State(state): State<ManagementState>,
    Json(body): Json<OAuthSetupRequest>,
) -> impl IntoResponse {
    use crate::oauth::dcr;
    use crate::oauth::discovery;

    let Some(ref setup_mgr) = state.setup_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Setup manager not available",
            None,
        )
        .into_response();
    };

    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            None,
        )
        .into_response();
    };

    // Check for duplicate name in existing config and snapshot the
    // SSRF opt-out flag for this request.
    let allow_insecure_oauth = {
        let config = state.config.read().await;
        if config.endpoints.iter().any(|e| e.name == body.name) {
            return error_response(
                StatusCode::CONFLICT,
                "endpoint_exists",
                Some(&format!(
                    "An endpoint named '{}' already exists. Use a different name.",
                    body.name
                )),
            )
            .into_response();
        }
        config.relay.allow_insecure_oauth.unwrap_or(false)
    };

    let scopes_str = body.scopes.as_ref().map(|s| s.join(" "));
    let Some(session_id) = setup_mgr
        .create_session(
            body.name.clone(),
            body.url.clone(),
            scopes_str.clone(),
            body.tool_prefix.clone(),
            body.server_type_override.clone(),
        )
        .await
    else {
        // The name is reserved by another live setup session. Rejecting here
        // (atomically, under the session-map lock) prevents two same-name
        // setups from racing: the loser's setup-time DCR save could otherwise
        // overwrite the winner's validated record between the commit-time
        // store check and the config write.
        return error_response(
            StatusCode::CONFLICT,
            "setup_in_progress",
            Some(&format!(
                "Another setup session for '{}' is already in progress. \
                 Commit or cancel it first, or wait for it to expire.",
                body.name
            )),
        )
        .into_response();
    };

    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);

    // ── Step 1: Discover OAuth server ──────────────────────────────────
    // If the caller supplied an explicit `oauth_server_url`, use that as the
    // discovery base instead of the resource URL — this lets users point at an
    // authorization server that doesn't expose RFC 9728 protected-resource
    // metadata on the resource URL itself.
    let discovery_base = body
        .oauth_server_url
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(body.url.as_str());
    let disc = match discovery::discover_oauth_server(discovery_base, allow_insecure_oauth).await {
        Ok(d) => d,
        Err(e) => {
            // Clean up session on failure
            setup_mgr.remove_session(&session_id).await;
            return error_response(
                StatusCode::BAD_REQUEST,
                "discovery_failed",
                Some(&format!("Could not discover OAuth server. Details: {e}")),
            )
            .into_response();
        }
    };

    // Store discovered metadata in the session
    let auth_endpoint = disc.authorization_endpoint.clone();
    let token_endpoint = disc.token_endpoint.clone();
    let registration_endpoint = disc.registration_endpoint.clone();
    let discovered_scopes = disc.scopes_supported.clone();
    let auth_server_url = disc.auth_server_url.clone();
    let issuer = disc.issuer.clone();

    setup_mgr
        .get_session_mut(&session_id, |s| {
            s.authorization_endpoint = Some(disc.authorization_endpoint.clone());
            s.token_endpoint = Some(disc.token_endpoint.clone());
            s.registration_endpoint = disc.registration_endpoint.clone();
            s.oauth_server_url = Some(disc.auth_server_url.clone());
            s.issuer = Some(disc.issuer.clone());
        })
        .await;

    // ── Step 2: Resolve client credentials ─────────────────────────────
    // If the caller already supplied a client_id, skip DCR entirely and use
    // the provided credentials. This mirrors the DCR success path: persist
    // via the token manager and proceed straight to authorize URL building.
    let manual_client_id = body
        .client_id
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let manual_client_secret = body
        .client_secret
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Resolve client credentials through the shared fallback chain:
    // explicit manual → pre-registered → CIMD → DCR. The Add Server flow does
    // not consult stored credentials (always a fresh setup), so `preregistered`
    // is `None`; an explicit pasted client_id wins, otherwise a CIMD-advertising
    // AS yields a zero-config public client and DCR is the final fallback.
    let manual = manual_client_id
        .clone()
        .map(|id| (id, manual_client_secret.clone()));
    let dcr_redirect_uri = redirect_uri.clone();
    let dcr_endpoint_name = body.name.clone();
    let resolve_result = client::resolve_client(
        client::ClientInputs {
            explicit_manual: manual,
            preregistered: None,
            cimd_supported: disc.client_id_metadata_document_supported,
            registration_endpoint: registration_endpoint.clone(),
        },
        |reg_endpoint| async move {
            let resp = dcr::register_client(
                &reg_endpoint,
                &dcr_redirect_uri,
                &dcr_endpoint_name,
                allow_insecure_oauth,
            )
            .await?;
            Ok::<_, dcr::DcrError>(client::DcrOutcome {
                client_id: resp.client_id,
                client_secret: resp.client_secret,
                client_secret_expires_at: resp.client_secret_expires_at,
            })
        },
    )
    .await
    .map_err(|e| match e {
        client::ClientResolveError::Dcr(e) => format!("{e}"),
        client::ClientResolveError::NoCredentials => {
            "No registration endpoint available".to_string()
        }
    });

    match resolve_result {
        Ok(resolved) => {
            let client_id = resolved.client_id;
            let client_secret = resolved.client_secret;
            let client_registration = resolved.registration;
            // Only true DCR registration counts as DCR for the response flag;
            // CIMD and manual paths do not.
            let used_dcr = matches!(client_registration, ClientRegistration::Dcr);

            // Persist resolved credentials so future re-auth can find them.
            // Manual creds are user-managed (not issuer-bound); DCR/CIMD creds
            // are bound to the discovered issuer.
            if let Some(ref tm) = state.token_manager {
                let creds = DcrCredentials {
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    client_secret_expires_at: resolved.client_secret_expires_at,
                    registered_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    issuer: match client_registration {
                        ClientRegistration::Manual => None,
                        _ => Some(issuer.clone()),
                    },
                    // Only true DCR (RFC 7591) counts as DCR provenance;
                    // CIMD and manual paths are never auto-discarded.
                    registered_via_dcr: used_dcr,
                    ..Default::default()
                };
                if let Err(e) = tm.save_dcr(&body.name, &creds).await {
                    warn!(endpoint = %body.name, error = %e, "Failed to persist client credentials");
                }
            }

            // Store credentials in session
            setup_mgr
                .get_session_mut(&session_id, |s| {
                    s.client_id = Some(client_id.clone());
                    s.client_secret = client_secret.clone();
                    s.client_secret_expires_at = resolved.client_secret_expires_at;
                    s.registered_via_dcr = used_dcr;
                    s.status = crate::oauth::SetupSessionStatus::AwaitingAuth;
                })
                .await;

            // Build PKCE + register flow
            let pkce = PkceChallenge::generate();
            let code_challenge = pkce.code_challenge.clone();

            let state_param = flow_mgr
                .start_flow(
                    &format!("setup:{}", session_id),
                    &token_endpoint,
                    &client_id,
                    client_secret.as_deref(),
                    pkce,
                    &redirect_uri,
                    Some(issuer.as_str()),
                    disc.authorization_response_iss_parameter_supported,
                )
                .await;

            let mut authorize_url = format!(
                "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
                auth_endpoint,
                urlencoding(&client_id),
                urlencoding(&redirect_uri),
                urlencoding(&state_param),
                urlencoding(&code_challenge),
            );

            // Google needs `access_type=offline` for a refresh token
            // (shared helper); setup-created Google endpoints need it too or
            // their very first grant is access-token-only.
            append_google_authorize_params(&mut authorize_url, &auth_endpoint);

            // Scope accumulation: union any previously-granted scopes (from a
            // persisted TokenSet for this name) with the requested scopes.
            let requested_scope = scopes_str.clone().unwrap_or_default();
            let prior_scope = if let Some(ref tm) = state.token_manager {
                tm.load(&body.name)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|t| t.scope)
            } else {
                None
            };
            let merged_scope = merge_scopes(prior_scope.as_deref(), &requested_scope);
            if !merged_scope.is_empty() {
                authorize_url.push_str(&format!("&scope={}", urlencoding(&merged_scope)));
            }

            info!(
                endpoint = %body.name,
                client_registration = client_registration.as_str(),
                "OAuth setup: authorize URL composed"
            );

            let discovery_info = OAuthDiscoveryInfo {
                auth_server: auth_server_url,
                dcr_used: used_dcr,
                scopes_available: discovered_scopes,
            };

            Json(OAuthSetupResponse {
                session_id: session_id.to_string(),
                status: "awaiting_auth".to_string(),
                authorize_url: Some(authorize_url),
                discovery: Some(discovery_info),
                dcr_error: None,
            })
            .into_response()
        }
        Err(dcr_err) => {
            // DCR failed — return session_id so desktop can supply manual creds
            let discovery_info = OAuthDiscoveryInfo {
                auth_server: auth_server_url,
                dcr_used: false,
                scopes_available: discovered_scopes,
            };

            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(OAuthSetupResponse {
                    session_id: session_id.to_string(),
                    status: "awaiting_credentials".to_string(),
                    authorize_url: None,
                    discovery: Some(discovery_info),
                    dcr_error: Some(dcr_err),
                }),
            )
                .into_response()
        }
    }
}

/// POST /api/oauth/setup/:id/credentials
///
/// Provide manual client credentials for a setup session, then return
/// the authorization URL.
async fn oauth_setup_credentials(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
    Json(body): Json<OAuthCredentialsRequest>,
) -> impl IntoResponse {
    let session_id: uuid::Uuid = match id.parse() {
        Ok(u) => u,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid session_id", None)
                .into_response()
        }
    };

    let Some(ref setup_mgr) = state.setup_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Setup manager not available",
            None,
        )
        .into_response();
    };

    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            None,
        )
        .into_response();
    };

    if body.client_id.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "client_id must not be empty", None)
            .into_response();
    }

    // Extract session data needed for building auth URL
    let session_data = setup_mgr
        .get_session_mut(&session_id, |s| {
            s.client_id = Some(body.client_id.clone());
            s.client_secret = body.client_secret.clone();
            // Manually-supplied credentials never expire on their own.
            s.client_secret_expires_at = 0;
            // Manually-supplied credentials are never DCR-provenanced.
            s.registered_via_dcr = false;
            s.status = crate::oauth::SetupSessionStatus::AwaitingAuth;
            (
                s.authorization_endpoint.clone(),
                s.token_endpoint.clone(),
                s.scopes.clone(),
                s.name.clone(),
                s.issuer.clone(),
            )
        })
        .await;

    let Some((Some(auth_endpoint), Some(token_endpoint), scopes, name, issuer)) = session_data
    else {
        return error_response(StatusCode::NOT_FOUND, "session not found or expired", None)
            .into_response();
    };

    // Persist manual credentials
    if let Some(ref tm) = state.token_manager {
        let creds = DcrCredentials {
            client_id: body.client_id.clone(),
            client_secret: body.client_secret.clone(),
            client_secret_expires_at: 0,
            registered_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            // Manually-supplied credentials are user-managed; not issuer-bound.
            issuer: None,
            // Manual credentials from the setup session → never auto-discard.
            registered_via_dcr: false,
            ..Default::default()
        };
        if let Err(e) = tm.save_dcr(&name, &creds).await {
            warn!(endpoint = %name, error = %e, "Failed to persist manual credentials");
        }
    }

    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);

    let pkce = PkceChallenge::generate();
    let code_challenge = pkce.code_challenge.clone();
    let state_param = flow_mgr
        .start_flow(
            &format!("setup:{}", session_id),
            &token_endpoint,
            &body.client_id,
            body.client_secret.as_deref(),
            pkce,
            &redirect_uri,
            issuer.as_deref(),
            // Manual-credential path: no RFC 9207 advertisement was threaded
            // through the setup session, so tolerate a missing callback `iss`.
            false,
        )
        .await;

    let mut authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        auth_endpoint,
        urlencoding(&body.client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state_param),
        urlencoding(&code_challenge),
    );

    // Google needs `access_type=offline` for a refresh token (shared
    // helper); the manual-credentials setup path needs it too or the very
    // first grant is access-token-only.
    append_google_authorize_params(&mut authorize_url, &auth_endpoint);

    // Scope accumulation: union previously-granted scopes (from a persisted
    // TokenSet for this endpoint) with the requested scopes for step-up.
    let requested_scope = scopes.clone().unwrap_or_default();
    let prior_scope = if let Some(ref tm) = state.token_manager {
        tm.load(&name).await.ok().flatten().and_then(|t| t.scope)
    } else {
        None
    };
    let merged_scope = merge_scopes(prior_scope.as_deref(), &requested_scope);
    if !merged_scope.is_empty() {
        authorize_url.push_str(&format!("&scope={}", urlencoding(&merged_scope)));
    }

    Json(serde_json::json!({
        "status": "awaiting_auth",
        "authorize_url": authorize_url
    }))
    .into_response()
}

/// GET /api/oauth/setup/:id/status
///
/// Poll the status of a setup session.
async fn oauth_setup_status(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id: uuid::Uuid = match id.parse() {
        Ok(u) => u,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid session_id", None)
                .into_response()
        }
    };

    let Some(ref setup_mgr) = state.setup_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Setup manager not available",
            None,
        )
        .into_response();
    };

    let result = setup_mgr
        .get_session(&session_id, |s| {
            let status = match s.status {
                crate::oauth::SetupSessionStatus::AwaitingCredentials => "awaiting_credentials",
                crate::oauth::SetupSessionStatus::AwaitingAuth => "awaiting_auth",
                crate::oauth::SetupSessionStatus::Authorized => "authorized",
                crate::oauth::SetupSessionStatus::Committing => "committing",
            };
            OAuthSetupStatusResponse {
                session_id: session_id.to_string(),
                status: status.to_string(),
                name: s.name.clone(),
                url: s.url.clone(),
            }
        })
        .await;

    match result {
        Some(resp) => Json(resp).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "session not found or expired", None)
            .into_response(),
    }
}

/// POST /api/oauth/setup/:id/commit
///
/// Write the fully-configured endpoint to config.toml and register the adapter.
/// Only succeeds if the session has status `Authorized`.
///
/// The commit atomically claims the session
/// ([`OAuthSetupManager::claim_for_commit`]): while claimed, a duplicate
/// commit gets `409 commit_in_progress` and a concurrent cancel cannot
/// remove the session, so exactly one request can consume it. A failed
/// commit releases the claim, keeping the session for retry or cancel.
///
/// The `client_secret` is never written to config.toml — client credentials
/// live in the DCR store (`{name}.dcr.json` via `TokenManager`). Only the
/// non-secret `client_id` is stamped into the `[[endpoints]]` entry. If no
/// DCR record was persisted during setup, the session's credentials are
/// defensively saved to the store (atomic save-if-absent under the DCR
/// write lock, carrying the session's registration provenance and secret
/// expiry) before the config is written. A pre-existing record that does
/// not match the session's credentials rejects the commit with
/// `409 dcr_record_mismatch`; a store read/write failure rejects it with
/// `500 dcr_persistence_failed` (a secretless TOML entry with no readable
/// store record would be unusable after restart).
///
/// The committed endpoint is applied through the same synchronous path as
/// `POST /api/endpoints` ([`apply_endpoint_change`]): the TOML write, the
/// adapter registration, and the `state.config` publish all complete
/// before the session (and its name reservation) is released, so a new
/// same-name setup attempt can never slip in between the config write and
/// the watcher's debounced reload.
async fn oauth_setup_commit(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id: uuid::Uuid = match id.parse() {
        Ok(u) => u,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid session_id", None)
                .into_response()
        }
    };

    let Some(ref setup_mgr) = state.setup_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Setup manager not available",
            None,
        )
        .into_response();
    };

    // Atomically claim the session for this commit. The claim (a) keeps the
    // session in the manager so its name reservation (enforced by
    // `create_session`) blocks a concurrent same-name setup from starting
    // and overwriting the DCR record validated below, and (b) locks out a
    // concurrent cancel or duplicate commit for the whole commit window.
    let session = match setup_mgr.claim_for_commit(&session_id).await {
        crate::oauth::CommitClaim::Claimed(s) => *s,
        crate::oauth::CommitClaim::AlreadyCommitting => {
            return error_response(
                StatusCode::CONFLICT,
                "commit_in_progress",
                Some("Another commit request for this session is in flight."),
            )
            .into_response();
        }
        crate::oauth::CommitClaim::NotAuthorized => {
            return error_response(
                StatusCode::CONFLICT,
                "session_not_authorized",
                Some("OAuth authorization has not been completed yet."),
            )
            .into_response();
        }
        crate::oauth::CommitClaim::NotFound => {
            return error_response(StatusCode::NOT_FOUND, "session not found or expired", None)
                .into_response();
        }
    };

    match commit_claimed_session(&state, &session).await {
        Ok(()) => {}
        Err(resp) => {
            // Failed commit: revert the claim so the session can be retried
            // or cancelled.
            setup_mgr.release_commit_claim(&session_id).await;
            return *resp;
        }
    }

    // Commit succeeded — consume the session. Its name reservation is only
    // released now, after `commit_claimed_session` has published the
    // committed config to `state.config`, so the setup-start duplicate
    // check can already see the new endpoint. (The session's tokens were
    // persisted inside `commit_claimed_session`, before the adapter was
    // registered.)
    setup_mgr.remove_session(&session_id).await;

    Json(serde_json::json!({
        "status": "committed",
        "name": session.name
    }))
    .into_response()
}

/// Fallible body of [`oauth_setup_commit`], run while the session is claimed
/// (`Committing`). Persists the session's client credentials to the DCR
/// store (defensive save-if-absent) and its tokens, then writes and
/// synchronously applies the committed endpoint. Any `Err` leaves the
/// session claimed for the caller to release.
async fn commit_claimed_session(
    state: &ManagementState,
    session: &crate::oauth::OAuthSetupSession,
) -> Result<(), Box<axum::response::Response>> {
    if state.config_path.is_none() {
        return Err(Box::new(
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_path not configured",
                None,
            )
            .into_response(),
        ));
    }

    // Defensive persistence: if the setup flow did not write a DCR record
    // (e.g. an older/partial flow), save the session's client credentials to
    // the store now so they survive without a TOML `client_secret`. The
    // save-if-absent runs atomically under the DCR write lock (`update_dcr`),
    // so a record written concurrently between commit's read and write can
    // never be clobbered. When a record already exists it must carry this
    // session's credentials: a mismatched record (stale file left behind by
    // an endpoint deletion, or a same-name session) would shadow the
    // committed `client_id` at credential-resolution time, so the commit is
    // rejected before anything is written to config.toml.
    if let (Some(ref tm), Some(ref cid)) = (&state.token_manager, &session.client_id) {
        enum DcrCommitError {
            Mismatch { stored_client_id: String },
            Token(TokenError),
        }
        impl From<TokenError> for DcrCommitError {
            fn from(e: TokenError) -> Self {
                DcrCommitError::Token(e)
            }
        }

        let outcome = tm
            .update_dcr(&session.name, |existing| match existing {
                None => Ok(Some(DcrCredentials {
                    client_id: cid.clone(),
                    client_secret: session.client_secret.clone(),
                    // The expiry resolved during setup (DCR response's
                    // `client_secret_expires_at`; 0 for manual creds), so a
                    // recovered record keeps the same lifetime the normal
                    // setup-time save would have written.
                    client_secret_expires_at: session.client_secret_expires_at,
                    registered_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    // Issuer binding follows provenance, matching the setup
                    // paths: DCR-minted credentials are bound to the issuer
                    // they were registered against (issuer-mismatch
                    // invalidation applies), while manually supplied ones
                    // are stored issuer-unbound (`None`), same as the
                    // /credentials path.
                    issuer: if session.registered_via_dcr {
                        session.issuer.clone()
                    } else {
                        None
                    },
                    // Provenance tracked on the session: true only when the
                    // credentials were minted via RFC 7591 DCR during setup,
                    // so recovered records keep self-heal eligibility.
                    registered_via_dcr: session.registered_via_dcr,
                    ..Default::default()
                })),
                Some(existing) => {
                    if existing.client_id == *cid && existing.client_secret == session.client_secret
                    {
                        // Existing record matches this session — keep it
                        // (preserves registered_at/issuer/provenance and any
                        // resource credentials). `update_dcr` skips the save
                        // when the returned record equals the loaded one, so
                        // this does not rewrite the file.
                        Ok(Some(existing))
                    } else {
                        Err(DcrCommitError::Mismatch {
                            stored_client_id: existing.client_id,
                        })
                    }
                }
            })
            .await;

        match outcome {
            Ok(_) => {}
            Err(DcrCommitError::Mismatch { stored_client_id }) => {
                warn!(
                    endpoint = %session.name,
                    stored_client_id = %stored_client_id,
                    "Stored DCR record does not match the setup session's credentials; refusing to commit"
                );
                return Err(Box::new(
                    error_response(
                        StatusCode::CONFLICT,
                        "dcr_record_mismatch",
                        Some(
                            "A stored credential record for this endpoint name does not \
                             match this setup session's client credentials. Delete the \
                             endpoint's stored credentials or re-run setup under a \
                             different name.",
                        ),
                    )
                    .into_response(),
                ));
            }
            Err(DcrCommitError::Token(e)) => {
                // A load failure (corrupt record) or save failure means we
                // cannot prove the store holds this session's credentials.
                // Committing anyway would write a secretless TOML entry that
                // resolves to no usable credentials after restart, so fail
                // the commit; the session (and its name reservation) survive
                // for a retry or cancel.
                warn!(
                    endpoint = %session.name,
                    error = %e,
                    "Failed to read/write DCR credentials at commit; refusing to commit"
                );
                return Err(Box::new(
                    error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "dcr_persistence_failed",
                        Some(&format!(
                            "Could not persist client credentials to the credential \
                             store: {e}. The setup session was kept; retry the commit \
                             or cancel the session.",
                        )),
                    )
                    .into_response(),
                ));
            }
        }
    }

    // Persist the session's tokens BEFORE the endpoint is applied below:
    // `apply_endpoint_change` registers the new adapter, whose spawned
    // initialization immediately loads the token from disk — saving only
    // after the apply returns would race that load and could strand a
    // freshly authorized adapter in NeedsLogin. A token saved ahead of a
    // commit that then fails is harmless: a retry overwrites it, and a
    // token file without a config entry grants nothing.
    if let (Some(ref tm), Some(ref tokens)) = (&state.token_manager, &session.tokens) {
        if let Err(e) = tm.save(&session.name, tokens).await {
            warn!(endpoint = %session.name, error = %e, "Failed to persist tokens");
        }
    }

    // Build the committed endpoint and apply it through the same synchronous
    // path as `POST /api/endpoints`: TOML write + adapter registration +
    // `state.config` publish, all before this function returns. Publishing
    // synchronously (instead of waiting for the watcher's debounced reload)
    // closes the window in which a new same-name setup could pass the
    // setup-start duplicate check against a stale `state.config` snapshot.
    // `client_secret` is deliberately NOT part of the entry; it lives in the
    // DCR store only (persisted above).
    let new_ep = crate::config::EndpointConfig {
        name: session.name.clone(),
        description: None,
        tool_prefix: session.tool_prefix.clone(),
        transport: crate::config::Transport::Oauth,
        command: None,
        args: None,
        url: Some(session.url.clone()),
        env: None,
        headers: None,
        disabled: false,
        disabled_tools: Vec::new(),
        oauth_server_url: session.oauth_server_url.clone(),
        client_id: session.client_id.clone(),
        client_secret: None,
        scopes: session
            .scopes
            .as_ref()
            .map(|s| {
                s.split_whitespace()
                    .map(|scope| scope.to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty()),
        token_endpoint: session.token_endpoint.clone(),
        server_type_override: session.server_type_override.clone(),
        isolation: None,
        container_image: None,
        mounts: None,
        auth: None,
    };

    apply_endpoint_change(state, new_ep, None).await
}

/// DELETE /api/oauth/setup/:id
///
/// Cancel a setup session. Cleans up without writing to config.
async fn oauth_setup_cancel(
    State(state): State<ManagementState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let session_id: uuid::Uuid = match id.parse() {
        Ok(u) => u,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid session_id", None)
                .into_response()
        }
    };

    let Some(ref setup_mgr) = state.setup_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Setup manager not available",
            None,
        )
        .into_response();
    };

    match setup_mgr.cancel_session(&session_id).await {
        crate::oauth::CancelOutcome::Cancelled => {
            Json(serde_json::json!({ "status": "cancelled" })).into_response()
        }
        crate::oauth::CancelOutcome::CommitInProgress => error_response(
            StatusCode::CONFLICT,
            "commit_in_progress",
            Some("A commit for this session is in flight; it cannot be cancelled."),
        )
        .into_response(),
        crate::oauth::CancelOutcome::NotFound => {
            error_response(StatusCode::NOT_FOUND, "session not found or expired", None)
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Profile CRUD (R4.A)
// ---------------------------------------------------------------------------

/// Request body for `POST /api/profiles` and `PUT /api/profiles/{path}`.
///
/// `js_execution` and `toon_output` are required: requests that omit (or
/// send `null` for) either field are rejected at deserialization time by
/// the axum `Json` extractor.
#[derive(Deserialize)]
pub struct ProfileRequest {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub endpoints: Vec<String>,
    pub js_execution: bool,
    pub toon_output: bool,
}

/// Summary shape returned by `GET /api/profiles` and `POST /api/profiles`.
/// Mirrors the JSON shape documented in Engineering Spec §8.2.
#[derive(Serialize)]
pub struct ProfileSummary {
    pub name: String,
    pub path: String,
    pub endpoints: Vec<String>,
    /// Per-profile JS-execution flag (mirror of
    /// [`crate::config::ProfileConfig::js_execution`]).
    pub js_execution: bool,
    /// Per-profile TOON output flag (mirror of
    /// [`crate::config::ProfileConfig::toon_output`]).
    pub toon_output: bool,
    pub endpoint_count: usize,
    pub tool_count: usize,
}

/// Detail shape returned by `GET /api/profiles/{path}` — same fields as
/// [`ProfileSummary`] plus the profile-scoped tool catalog.
#[derive(Serialize)]
pub struct ProfileDetail {
    #[serde(flatten)]
    pub summary: ProfileSummary,
    pub tools: Vec<CatalogEntry>,
}

/// Look up the live tool count for a profile (post-rebuild). Returns 0 when
/// the registry isn't available (legacy test fixtures).
async fn profile_tool_count(state: &ManagementState, path: &str) -> usize {
    let Some(ref pr) = state.profile_registry else {
        return 0;
    };
    match pr.get(path).await {
        Some(ctx) => ctx.registry_view.merged_catalog().await.len(),
        None => 0,
    }
}

/// Build a [`ProfileSummary`] from a [`crate::config::ProfileConfig`].
async fn build_profile_summary(
    state: &ManagementState,
    profile: &crate::config::ProfileConfig,
) -> ProfileSummary {
    let tool_count = profile_tool_count(state, &profile.path).await;
    ProfileSummary {
        name: profile.name.clone(),
        path: profile.path.clone(),
        endpoints: profile.endpoints.clone(),
        js_execution: profile.js_execution,
        toon_output: profile.toon_output,
        endpoint_count: profile.endpoints.len(),
        tool_count,
    }
}

/// Validate a request body for create / update. Returns a 400 response on
/// failure; on success returns the [`crate::config::ProfileConfig`] that
/// would land in `config.toml`.
fn validate_profile_request(
    req: &ProfileRequest,
    cfg: &Config,
    existing_path_lower: Option<&str>,
) -> Result<crate::config::ProfileConfig, Box<axum::http::Response<axum::body::Body>>> {
    let bad_request = |err: &str, detail: String| -> Box<axum::http::Response<axum::body::Body>> {
        Box::new(error_response(StatusCode::BAD_REQUEST, err, Some(&detail)).into_response())
    };
    let conflict = |err: &str, detail: String| -> Box<axum::http::Response<axum::body::Body>> {
        Box::new(error_response(StatusCode::CONFLICT, err, Some(&detail)).into_response())
    };

    if req.name.trim().is_empty() {
        return Err(bad_request(
            "invalid profile",
            "Profile name must not be empty".to_string(),
        ));
    }
    if let Err(msg) = crate::config::validate_profile_path(&req.path) {
        return Err(bad_request("invalid profile path", msg));
    }
    let new_path_lower = req.path.to_ascii_lowercase();
    // Uniqueness: scan current profiles, ignoring the profile we're updating
    // in place (matched by its current path, case-insensitive).
    if let Some(profiles) = cfg.profiles.as_ref() {
        for p in profiles {
            let p_lower = p.path.to_ascii_lowercase();
            if Some(p_lower.as_str()) == existing_path_lower {
                continue;
            }
            if p_lower == new_path_lower {
                return Err(conflict(
                    "duplicate profile path",
                    format!(
                        "Profile path '{}' is already in use (paths are case-insensitive).",
                        req.path
                    ),
                ));
            }
            if p.name == req.name && existing_path_lower != Some(p_lower.as_str()) {
                return Err(conflict(
                    "duplicate profile name",
                    format!("Profile name '{}' is already in use.", req.name),
                ));
            }
        }
    }
    let endpoint_names: std::collections::HashSet<&str> =
        cfg.endpoints.iter().map(|e| e.name.as_str()).collect();
    for ep in &req.endpoints {
        if !endpoint_names.contains(ep.as_str()) {
            return Err(bad_request(
                "unknown endpoint",
                format!(
                    "Profile '{}' references unknown endpoint '{}'",
                    req.name, ep
                ),
            ));
        }
    }
    Ok(crate::config::ProfileConfig {
        name: req.name.clone(),
        path: req.path.clone(),
        endpoints: req.endpoints.clone(),
        js_execution: req.js_execution,
        toon_output: req.toon_output,
    })
}

/// Persist the updated profile list back to `config.toml`, preserving the
/// rest of the document. Mirrors the targeted-edit pattern used by
/// [`delete_endpoint`] / [`oauth_setup_commit`].
fn write_profiles_to_disk(
    config_path: &std::path::Path,
    profiles: &[crate::config::ProfileConfig],
) -> Result<(), (StatusCode, &'static str, String)> {
    let contents = std::fs::read_to_string(config_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read config file",
            e.to_string(),
        )
    })?;
    let mut parsed: toml::Table = contents.parse().map_err(|e: toml::de::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to parse config file",
            e.to_string(),
        )
    })?;

    if profiles.is_empty() {
        parsed.remove("profiles");
    } else {
        let arr: Vec<toml::Value> = profiles
            .iter()
            .map(|p| {
                toml::Value::try_from(p)
                    .expect("ProfileConfig is Serialize and round-trips through toml::Value")
            })
            .collect();
        parsed.insert("profiles".into(), toml::Value::Array(arr));
    }

    let new_contents = toml::to_string_pretty(&parsed).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to serialize config",
            e.to_string(),
        )
    })?;
    crate::config::write_config_file(config_path, &new_contents).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write config file",
            e.to_string(),
        )
    })?;
    Ok(())
}

/// Helper that writes `new_profiles` to disk, swaps them into the in-memory
/// [`ManagementState::config`], and rebuilds the live [`ProfileRegistry`] so
/// `/mcp/{profile}` reflects the change immediately. Returns the resolved
/// config path on success.
async fn apply_profiles_change(
    state: &ManagementState,
    new_profiles: Vec<crate::config::ProfileConfig>,
) -> Result<(), Box<axum::http::Response<axum::body::Body>>> {
    let Some(ref config_path) = state.config_path else {
        return Err(Box::new(
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_path not configured",
                Some("The management API was not initialised with a config file path."),
            )
            .into_response(),
        ));
    };
    let resolved = crate::config::expand_tilde(config_path);
    if let Err((status, err, detail)) = write_profiles_to_disk(&resolved, &new_profiles) {
        return Err(Box::new(
            error_response(status, err, Some(&detail)).into_response(),
        ));
    }
    // Swap the in-memory config baseline and rebuild the live registry.
    // R4.B will additionally rebuild on file-watcher events; for the
    // self-written change we do it inline so the response reflects the
    // post-write state without waiting on the watcher debounce.
    {
        let mut cfg = state.config.write().await;
        cfg.profiles = if new_profiles.is_empty() {
            None
        } else {
            Some(new_profiles.clone())
        };
    }
    if let Some(ref pr) = state.profile_registry {
        pr.rebuild(&new_profiles).await;
    }
    Ok(())
}

/// GET /api/profiles — list all profiles with resolved flags + live tool count.
async fn list_profiles(State(state): State<ManagementState>) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let profiles = cfg.profiles.clone().unwrap_or_default();
    drop(cfg);
    let mut out: Vec<ProfileSummary> = Vec::with_capacity(profiles.len());
    for p in &profiles {
        out.push(build_profile_summary(&state, p).await);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out).into_response()
}

/// POST /api/profiles — create a new profile.
async fn create_profile(
    State(state): State<ManagementState>,
    Json(req): Json<ProfileRequest>,
) -> impl IntoResponse {
    let cfg_snapshot = state.config.read().await.clone();
    let new_profile = match validate_profile_request(&req, &cfg_snapshot, None) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut profiles = cfg_snapshot.profiles.clone().unwrap_or_default();
    profiles.push(new_profile.clone());
    if let Err(resp) = apply_profiles_change(&state, profiles).await {
        return *resp;
    }
    let summary = build_profile_summary(&state, &new_profile).await;
    (StatusCode::CREATED, Json(summary)).into_response()
}

/// GET /api/profiles/{path} — read one profile (with full scoped catalog).
async fn get_profile(
    State(state): State<ManagementState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let path_lower = path.to_ascii_lowercase();
    let Some(profile) = cfg
        .profiles
        .as_ref()
        .and_then(|ps| {
            ps.iter()
                .find(|p| p.path.to_ascii_lowercase() == path_lower)
        })
        .cloned()
    else {
        return error_response(
            StatusCode::NOT_FOUND,
            "profile not found",
            Some(&format!("No profile at path '{}'", path)),
        )
        .into_response();
    };
    drop(cfg);
    let summary = build_profile_summary(&state, &profile).await;
    // Build a profile-scoped CatalogEntry list. When no profile_registry is
    // wired (legacy test fixtures), the catalog is empty.
    let tools: Vec<CatalogEntry> = if let Some(ref pr) = state.profile_registry {
        match pr.get(&profile.path).await {
            Some(ctx) => {
                let (tools, lookup) = ctx.registry_view.merged_catalog_with_lookup().await;
                let entries = state.registry.entries().read().await;
                tools
                    .into_iter()
                    .map(|t| {
                        let (endpoint_name, available) = match lookup.get(&t.name) {
                            Some((ep, _raw)) => {
                                let avail = entries
                                    .get(ep.as_str())
                                    .map(|e| {
                                        !e.disabled
                                            && matches!(
                                                e.adapter.health(),
                                                crate::adapter::HealthStatus::Healthy
                                            )
                                    })
                                    .unwrap_or(false);
                                (ep.clone(), avail)
                            }
                            None => ("unknown".to_string(), false),
                        };
                        CatalogEntry {
                            name: t.name,
                            description: t.description,
                            input_schema: t.input_schema,
                            annotations: t.annotations,
                            endpoint: endpoint_name,
                            available,
                        }
                    })
                    .collect()
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    Json(ProfileDetail { summary, tools }).into_response()
}

/// PUT /api/profiles/{path} — update / rename a profile.
async fn update_profile(
    State(state): State<ManagementState>,
    Path(path): Path<String>,
    Json(req): Json<ProfileRequest>,
) -> impl IntoResponse {
    let cfg_snapshot = state.config.read().await.clone();
    let path_lower = path.to_ascii_lowercase();
    let exists = cfg_snapshot
        .profiles
        .as_ref()
        .map(|ps| ps.iter().any(|p| p.path.to_ascii_lowercase() == path_lower))
        .unwrap_or(false);
    if !exists {
        return error_response(
            StatusCode::NOT_FOUND,
            "profile not found",
            Some(&format!("No profile at path '{}'", path)),
        )
        .into_response();
    }
    let updated = match validate_profile_request(&req, &cfg_snapshot, Some(&path_lower)) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut profiles = cfg_snapshot.profiles.clone().unwrap_or_default();
    for p in profiles.iter_mut() {
        if p.path.to_ascii_lowercase() == path_lower {
            *p = updated.clone();
            break;
        }
    }
    if let Err(resp) = apply_profiles_change(&state, profiles).await {
        return *resp;
    }
    let summary = build_profile_summary(&state, &updated).await;
    (StatusCode::OK, Json(summary)).into_response()
}

/// DELETE /api/profiles/{path} — remove a profile.
async fn delete_profile(
    State(state): State<ManagementState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let cfg_snapshot = state.config.read().await.clone();
    let path_lower = path.to_ascii_lowercase();
    let exists = cfg_snapshot
        .profiles
        .as_ref()
        .map(|ps| ps.iter().any(|p| p.path.to_ascii_lowercase() == path_lower))
        .unwrap_or(false);
    if !exists {
        return error_response(
            StatusCode::NOT_FOUND,
            "profile not found",
            Some(&format!("No profile at path '{}'", path)),
        )
        .into_response();
    }
    let profiles: Vec<_> = cfg_snapshot
        .profiles
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.path.to_ascii_lowercase() != path_lower)
        .collect();
    if let Err(resp) = apply_profiles_change(&state, profiles).await {
        return *resp;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// GET /api/endpoints/{name}/profiles — which profiles include this endpoint.
async fn get_endpoint_profile_membership(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let endpoint_exists = cfg.endpoints.iter().any(|e| e.name == name);
    if !endpoint_exists {
        return endpoint_not_found(&name).into_response();
    }
    let mut paths: Vec<String> = cfg
        .profiles
        .as_ref()
        .map(|ps| {
            ps.iter()
                .filter(|p| p.endpoints.iter().any(|ep| ep == &name))
                .map(|p| p.path.clone())
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    Json(serde_json::json!({ "profiles": paths })).into_response()
}

// ---------------------------------------------------------------------------
// Endpoint CRUD (issue #82) — POST /api/endpoints, PUT /api/endpoints/{name}
// ---------------------------------------------------------------------------

/// Request body for `POST /api/endpoints` and `PUT /api/endpoints/{name}`.
///
/// Mirrors the TOML fields the desktop's `add_endpoint` Tauri command writes
/// today. `client_secret` is deliberately accepted at the deserialization
/// layer so we can reject it with a clear 400 pointing at
/// `/api/endpoints/{name}/credentials`. `disabled` and `disabled_tools` are
/// rejected for the same reason — they have dedicated routes
/// (`/api/endpoints/{name}/{enable,disable}` and per-tool toggles).
#[derive(Deserialize)]
pub struct EndpointRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub transport: crate::config::Transport,
    #[serde(default)]
    pub tool_prefix: Option<String>,
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
    #[serde(default)]
    pub oauth_server_url: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    /// Space-separated string, matching the existing desktop `add_endpoint`
    /// Tauri shape (and `oauth_setup_commit`). Split on whitespace into a
    /// TOML array when persisted.
    #[serde(default)]
    pub scopes: Option<String>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub server_type_override: Option<String>,
    /// Isolation mode for stdio endpoints: `"container"` or `"none"`
    /// (default when omitted is direct spawn).
    #[serde(default)]
    pub isolation: Option<String>,
    /// OCI image override for containerized stdio endpoints.
    #[serde(default)]
    pub container_image: Option<String>,
    /// Host bind mounts (`"/host/path:/container/path"`) for containerized
    /// stdio endpoints.
    #[serde(default)]
    pub mounts: Option<Vec<String>>,
    /// Non-default auth binding for the endpoint (currently `type = "ema"`).
    /// Threaded into the persisted `[endpoints.auth]` sub-table so onboarding
    /// can create EMA endpoints via the management API.
    #[serde(default)]
    pub auth: Option<crate::config::EndpointAuthConfig>,
    // Forbidden fields — present so we can return a precise 400 instead of
    // silently dropping the value when a client mistakenly includes them.
    #[serde(default)]
    pub client_secret: Option<serde_json::Value>,
    #[serde(default)]
    pub disabled: Option<serde_json::Value>,
    #[serde(default)]
    pub disabled_tools: Option<serde_json::Value>,
}

/// Sanitized summary returned by `POST /api/endpoints` and
/// `PUT /api/endpoints/{name}`. Never contains a `client_secret` — that
/// value is write-only via `/api/endpoints/{name}/credentials`.
#[derive(Serialize)]
pub struct EndpointSummary {
    pub name: String,
    pub transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_server_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_type_override: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<String>>,
}

fn endpoint_summary_from(ep: &crate::config::EndpointConfig) -> EndpointSummary {
    EndpointSummary {
        name: ep.name.clone(),
        transport: ep.transport.to_string(),
        description: ep.description.clone(),
        tool_prefix: ep.tool_prefix.clone(),
        command: ep.command.clone(),
        args: ep.args.clone(),
        url: ep.url.clone(),
        env: ep.env.clone(),
        headers: ep.headers.clone(),
        oauth_server_url: ep.oauth_server_url.clone(),
        client_id: ep.client_id.clone(),
        scopes: ep.scopes.clone().unwrap_or_default(),
        token_endpoint: ep.token_endpoint.clone(),
        server_type_override: ep.server_type_override.clone(),
        isolation: ep.isolation.clone(),
        container_image: ep.container_image.clone(),
        mounts: ep.mounts.clone(),
    }
}

/// Validate an [`EndpointRequest`] against the same rules the config loader
/// enforces at startup (transport-specific required fields, tool_prefix
/// validity, name uniqueness, `server_type_override` sanitization), plus the
/// extra "forbidden-field" rejections specific to the management API. On
/// success returns the [`crate::config::EndpointConfig`] that will land in
/// `config.toml`.
///
/// `original_name` is `None` for create and `Some(orig)` for update — the
/// caller's original path-param name. When supplied, uniqueness scans skip
/// the entry currently identified by that name so renames work.
fn validate_endpoint_request(
    req: &EndpointRequest,
    cfg: &Config,
    original_name: Option<&str>,
) -> Result<crate::config::EndpointConfig, Box<axum::http::Response<axum::body::Body>>> {
    let bad_request = |err: &str, detail: String| -> Box<axum::http::Response<axum::body::Body>> {
        Box::new(error_response(StatusCode::BAD_REQUEST, err, Some(&detail)).into_response())
    };
    let conflict = |err: &str, detail: String| -> Box<axum::http::Response<axum::body::Body>> {
        Box::new(error_response(StatusCode::CONFLICT, err, Some(&detail)).into_response())
    };

    // Forbidden fields — `client_secret` belongs in the DCR file; disabled
    // state has dedicated routes.
    if req.client_secret.is_some() {
        return Err(bad_request(
            "client_secret not allowed",
            "POST the client_secret to /api/endpoints/{name}/credentials instead — it is persisted via TokenManager (DCR file, chmod 0600) rather than config.toml.".to_string(),
        ));
    }
    if req.disabled.is_some() {
        return Err(bad_request(
            "disabled not allowed",
            "Use POST /api/endpoints/{name}/disable or /enable instead.".to_string(),
        ));
    }
    if req.disabled_tools.is_some() {
        return Err(bad_request(
            "disabled_tools not allowed",
            "Use POST /api/endpoints/{name}/tools/{tool}/disable or /enable instead.".to_string(),
        ));
    }

    if req.name.trim().is_empty() {
        return Err(bad_request(
            "invalid endpoint",
            "Endpoint name must not be empty".to_string(),
        ));
    }

    // Transport-specific required fields (mirror Config::validate).
    match req.transport {
        crate::config::Transport::Stdio => {
            if req.command.as_deref().unwrap_or("").is_empty() {
                return Err(bad_request(
                    "invalid endpoint",
                    format!(
                        "Endpoint '{}': stdio transport requires a 'command' field",
                        req.name
                    ),
                ));
            }
        }
        crate::config::Transport::Sse
        | crate::config::Transport::Http
        | crate::config::Transport::Oauth => {
            if req.url.as_deref().unwrap_or("").is_empty() {
                return Err(bad_request(
                    "invalid endpoint",
                    format!(
                        "Endpoint '{}': {} transport requires a 'url' field",
                        req.name, req.transport
                    ),
                ));
            }
        }
    }

    // server_type_override — sanitize through the same rules adapters use
    // when reading the field at startup.
    let server_type_override = match req.server_type_override.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => {
            if let Err(e) = crate::adapter::server_name::sanitize_server_name(s) {
                return Err(bad_request(
                    "invalid server_type_override",
                    format!("Endpoint '{}': {}", req.name, e),
                ));
            }
            Some(s.to_string())
        }
        _ => None,
    };

    // isolation — must be "container" or "none" when present. Empty /
    // whitespace-only is treated as omitted (= direct spawn for stdio).
    let isolation = match req.isolation.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => {
            if s != "container" && s != "none" {
                return Err(bad_request(
                    "invalid isolation",
                    format!(
                        "Endpoint '{}': invalid isolation value '{}' — expected \"container\" or \"none\"",
                        req.name, s
                    ),
                ));
            }
            Some(s.to_string())
        }
        _ => None,
    };

    // Uniqueness (case-sensitive exact match, matching delete_endpoint and
    // today's desktop add_endpoint Tauri behaviour). For update, skip the
    // entry currently identified by `original_name`.
    for ep in &cfg.endpoints {
        if Some(ep.name.as_str()) == original_name {
            continue;
        }
        if ep.name == req.name {
            return Err(conflict(
                "duplicate endpoint name",
                format!("Endpoint name '{}' is already in use.", req.name),
            ));
        }
    }

    // Scopes: space-separated string → Vec<String>. Empty becomes None.
    let scopes_vec: Option<Vec<String>> = req.scopes.as_deref().and_then(|s| {
        let v: Vec<String> = s.split_whitespace().map(|s| s.to_string()).collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    });

    let new_ep = crate::config::EndpointConfig {
        name: req.name.clone(),
        description: req.description.clone(),
        tool_prefix: req.tool_prefix.clone(),
        transport: req.transport.clone(),
        command: req.command.clone(),
        args: req.args.clone(),
        url: req.url.clone(),
        env: req.env.clone(),
        headers: req.headers.clone(),
        disabled: false,
        disabled_tools: Vec::new(),
        oauth_server_url: req.oauth_server_url.clone(),
        client_id: req.client_id.clone(),
        client_secret: None,
        scopes: scopes_vec,
        token_endpoint: req.token_endpoint.clone(),
        server_type_override,
        isolation,
        container_image: req.container_image.clone(),
        mounts: req.mounts.clone(),
        auth: req.auth.clone(),
    };

    // Validate the `[endpoints.auth]` block (currently `type = "ema"`) using
    // the same rules the config loader applies, so an EMA body is rejected
    // unless it has a `resource` AND (`organization` OR `idp`).
    if let Err(msg) = crate::config::validate_endpoint_auth(&new_ep) {
        return Err(bad_request(
            "invalid auth",
            format!("Endpoint '{}': {}", req.name, msg),
        ));
    }

    Ok(new_ep)
}

/// Serialize an [`crate::config::EndpointConfig`] into a `toml::Table` with
/// a stable key order that mirrors today's `add_endpoint` Tauri command,
/// omitting empty / `None` fields. Disabled state and legacy `client_secret`
/// are intentionally NOT written here — the caller preserves them from the
/// existing TOML entry on update.
fn endpoint_to_toml_table(
    ep: &crate::config::EndpointConfig,
) -> toml::map::Map<String, toml::Value> {
    let mut t = toml::map::Map::new();
    t.insert("name".into(), toml::Value::String(ep.name.clone()));
    t.insert(
        "transport".into(),
        toml::Value::String(ep.transport.to_string()),
    );
    if let Some(ref tp) = ep.tool_prefix {
        t.insert("tool_prefix".into(), toml::Value::String(tp.clone()));
    }
    if let Some(ref cmd) = ep.command {
        t.insert("command".into(), toml::Value::String(cmd.clone()));
    }
    if let Some(ref args) = ep.args {
        let arr: Vec<toml::Value> = args
            .iter()
            .map(|a| toml::Value::String(a.clone()))
            .collect();
        t.insert("args".into(), toml::Value::Array(arr));
    }
    if let Some(ref url) = ep.url {
        t.insert("url".into(), toml::Value::String(url.clone()));
    }
    if let Some(ref desc) = ep.description {
        t.insert("description".into(), toml::Value::String(desc.clone()));
    }
    if let Some(ref env) = ep.env {
        if !env.is_empty() {
            let mut env_table = toml::map::Map::new();
            for (k, v) in env {
                env_table.insert(k.clone(), toml::Value::String(v.clone()));
            }
            t.insert("env".into(), toml::Value::Table(env_table));
        }
    }
    if let Some(ref headers) = ep.headers {
        if !headers.is_empty() {
            let mut headers_table = toml::map::Map::new();
            for (k, v) in headers {
                headers_table.insert(k.clone(), toml::Value::String(v.clone()));
            }
            t.insert("headers".into(), toml::Value::Table(headers_table));
        }
    }
    if let Some(ref os) = ep.oauth_server_url {
        t.insert("oauth_server_url".into(), toml::Value::String(os.clone()));
    }
    if let Some(ref cid) = ep.client_id {
        t.insert("client_id".into(), toml::Value::String(cid.clone()));
    }
    if let Some(ref scopes) = ep.scopes {
        if !scopes.is_empty() {
            let arr: Vec<toml::Value> = scopes
                .iter()
                .map(|s| toml::Value::String(s.clone()))
                .collect();
            t.insert("scopes".into(), toml::Value::Array(arr));
        }
    }
    if let Some(ref te) = ep.token_endpoint {
        t.insert("token_endpoint".into(), toml::Value::String(te.clone()));
    }
    if let Some(ref sto) = ep.server_type_override {
        t.insert(
            "server_type_override".into(),
            toml::Value::String(sto.clone()),
        );
    }
    if let Some(ref iso) = ep.isolation {
        t.insert("isolation".into(), toml::Value::String(iso.clone()));
    }
    if let Some(ref img) = ep.container_image {
        t.insert("container_image".into(), toml::Value::String(img.clone()));
    }
    if let Some(ref mounts) = ep.mounts {
        if !mounts.is_empty() {
            let arr: Vec<toml::Value> = mounts
                .iter()
                .map(|m| toml::Value::String(m.clone()))
                .collect();
            t.insert("mounts".into(), toml::Value::Array(arr));
        }
    }
    if let Some(ref auth) = ep.auth {
        let mut auth_table = toml::map::Map::new();
        auth_table.insert("type".into(), toml::Value::String(auth.auth_type.clone()));
        if let Some(ref org) = auth.organization {
            auth_table.insert("organization".into(), toml::Value::String(org.clone()));
        }
        if let Some(ref idp) = auth.idp {
            auth_table.insert("idp".into(), toml::Value::String(idp.clone()));
        }
        if let Some(ref resource) = auth.resource {
            auth_table.insert("resource".into(), toml::Value::String(resource.clone()));
        }
        t.insert("auth".into(), toml::Value::Table(auth_table));
    }
    t
}

/// Targeted edit of the on-disk `config.toml`: read, parse, replace
/// (update) or append (create) the matching `[[endpoints]]` entry,
/// serialize, and write back. Mirrors the pattern used by
/// [`delete_endpoint`] and [`oauth_setup_commit`] so unrelated TOML keys
/// and per-entry key order on other endpoints are preserved verbatim.
///
/// On update (`original_name = Some(orig)`), the existing entry's
/// `disabled`, `disabled_tools`, and legacy `client_secret` keys are
/// preserved — those are managed via dedicated routes / the credentials
/// endpoint, not the create/update body.
///
/// Name uniqueness is revalidated here against the on-disk entries: the
/// handlers' earlier checks ran against a possibly stale in-memory
/// snapshot, so a create (or rename to a new name) that would collide
/// with an existing entry fails with `409` instead of appending a
/// duplicate `[[endpoints]]` entry.
fn write_endpoint_to_disk(
    config_path: &std::path::Path,
    new_ep: &crate::config::EndpointConfig,
    original_name: Option<&str>,
) -> Result<(), (StatusCode, &'static str, String)> {
    let contents = std::fs::read_to_string(config_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read config file",
            e.to_string(),
        )
    })?;
    let mut parsed: toml::Table = contents.parse().map_err(|e: toml::de::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to parse config file",
            e.to_string(),
        )
    })?;

    let endpoints_val = parsed
        .entry("endpoints")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let arr = endpoints_val.as_array_mut().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to parse config file",
            "[[endpoints]] section is not an array".to_string(),
        )
    })?;

    let mut new_table = endpoint_to_toml_table(new_ep);

    let name_taken = |arr: &[toml::Value], name: &str| {
        arr.iter()
            .any(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
    };
    let duplicate_err = || {
        (
            StatusCode::CONFLICT,
            "duplicate endpoint name",
            format!("Endpoint name '{}' is already in use.", new_ep.name),
        )
    };

    match original_name {
        None => {
            if name_taken(arr, &new_ep.name) {
                return Err(duplicate_err());
            }
            arr.push(toml::Value::Table(new_table));
        }
        Some(orig) => {
            let idx = arr
                .iter()
                .position(|v| v.get("name").and_then(|n| n.as_str()) == Some(orig));
            let Some(idx) = idx else {
                return Err((
                    StatusCode::NOT_FOUND,
                    "endpoint not found",
                    format!("No endpoint named '{}'", orig),
                ));
            };
            if new_ep.name != orig && name_taken(arr, &new_ep.name) {
                return Err(duplicate_err());
            }
            if let Some(existing) = arr[idx].as_table() {
                for key in ["disabled", "disabled_tools", "client_secret"] {
                    if let Some(v) = existing.get(key) {
                        new_table.insert(key.into(), v.clone());
                    }
                }
            }
            arr[idx] = toml::Value::Table(new_table);
        }
    }

    let new_contents = toml::to_string_pretty(&parsed).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to serialize config",
            e.to_string(),
        )
    })?;
    crate::config::write_config_file(config_path, &new_contents).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write config file",
            e.to_string(),
        )
    })?;
    Ok(())
}

/// Persist `new_ep` to disk, swap the in-memory `ManagementState::config`
/// endpoints list, and rebuild the affected adapter inline so the response
/// reflects the post-write state without waiting on the file watcher.
/// Mirrors [`apply_profiles_change`] for the profile CRUD flow.
///
/// `original_name` is `None` for create and `Some(orig)` for update.
async fn apply_endpoint_change(
    state: &ManagementState,
    new_ep: crate::config::EndpointConfig,
    original_name: Option<&str>,
) -> Result<(), Box<axum::http::Response<axum::body::Body>>> {
    let Some(ref config_path) = state.config_path else {
        return Err(Box::new(
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_path not configured",
                Some("The management API was not initialised with a config file path."),
            )
            .into_response(),
        ));
    };
    let resolved = crate::config::expand_tilde(config_path);
    if let Err((status, err, detail)) = write_endpoint_to_disk(&resolved, &new_ep, original_name) {
        return Err(Box::new(
            error_response(status, err, Some(&detail)).into_response(),
        ));
    }

    // Build the new in-memory endpoints vec by replacing-or-appending and
    // preserving the existing entry's disabled / disabled_tools / legacy
    // client_secret on update so we match what we just wrote to disk.
    let old_cfg = state.config.read().await.clone();
    let mut new_endpoints = old_cfg.endpoints.clone();
    let preserved: Option<(bool, Vec<String>, Option<String>)> = match original_name {
        Some(orig) => {
            let idx = new_endpoints.iter().position(|e| e.name == orig);
            idx.map(|i| {
                let existing = &new_endpoints[i];
                (
                    existing.disabled,
                    existing.disabled_tools.clone(),
                    existing.client_secret.clone(),
                )
            })
        }
        None => None,
    };
    let mut effective = new_ep.clone();
    if let Some((dis, dis_tools, legacy_secret)) = preserved {
        effective.disabled = dis;
        effective.disabled_tools = dis_tools;
        effective.client_secret = legacy_secret;
    }
    match original_name {
        Some(orig) => {
            if let Some(slot) = new_endpoints.iter_mut().find(|e| e.name == orig) {
                *slot = effective.clone();
            } else {
                new_endpoints.push(effective.clone());
            }
        }
        None => {
            new_endpoints.push(effective.clone());
        }
    }

    // Build the new Config (preserves relay + profiles untouched), diff
    // against the old one, and feed it through the same `apply_diff_graceful`
    // path the file watcher / reload_config use. This restarts only the
    // affected adapter when fields it cares about changed.
    let new_cfg = Config {
        relay: old_cfg.relay.clone(),
        endpoints: new_endpoints.clone(),
        profiles: old_cfg.profiles.clone(),
        organizations: old_cfg.organizations.clone(),
    };
    let diff = crate::config::diff_configs(&old_cfg, &new_cfg);
    let token_manager = state.token_manager.clone().unwrap_or_else(|| {
        Arc::new(crate::token_manager::TokenManager::new(
            std::path::PathBuf::from("/tmp"),
        ))
    });
    let oauth_adapter_inners = state
        .oauth_adapter_inners
        .clone()
        .unwrap_or_else(|| Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())));
    let jit = state
        .oauth_flow_manager
        .as_ref()
        .map(|fm| crate::watcher::JitWiring {
            relay_port: state.relay_port,
            flow_manager: fm.clone(),
        });
    crate::watcher::apply_diff_graceful(
        &diff,
        &state.registry,
        &[],
        &std::collections::HashSet::new(),
        &token_manager,
        &oauth_adapter_inners,
        new_cfg.relay.allow_insecure_oauth.unwrap_or(false),
        state.event_bus.as_ref(),
        jit.as_ref(),
        &new_cfg.organizations,
    )
    .await;

    *state.config.write().await = new_cfg;
    Ok(())
}

/// POST /api/endpoints — create a new endpoint.
async fn create_endpoint(
    State(state): State<ManagementState>,
    Json(req): Json<EndpointRequest>,
) -> impl IntoResponse {
    let cfg_snapshot = state.config.read().await.clone();
    let new_ep = match validate_endpoint_request(&req, &cfg_snapshot, None) {
        Ok(ep) => ep,
        Err(resp) => return *resp,
    };
    if let Some(resp) = name_reserved_by_setup(&state, &new_ep.name).await {
        return *resp;
    }
    if let Err(resp) = apply_endpoint_change(&state, new_ep.clone(), None).await {
        return *resp;
    }
    (StatusCode::CREATED, Json(endpoint_summary_from(&new_ep))).into_response()
}

/// `409` when `name` is currently reserved by a live OAuth setup session.
/// The regular create/rename APIs consult this so they cannot take a name
/// mid-setup and later collide with the session's own commit (whose claim
/// keeps the reservation alive until the committed config is published).
async fn name_reserved_by_setup(
    state: &ManagementState,
    name: &str,
) -> Option<Box<axum::response::Response>> {
    let setup_mgr = state.setup_manager.as_ref()?;
    if setup_mgr.is_name_reserved(name).await {
        return Some(Box::new(
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "duplicate endpoint name",
                    "detail": format!(
                        "Endpoint name '{}' is reserved by an OAuth setup session in progress.",
                        name
                    ),
                })),
            )
                .into_response(),
        ));
    }
    None
}

/// PUT /api/endpoints/{name} — update (and optionally rename) an endpoint.
///
/// The path parameter identifies the existing endpoint; the body's `name`
/// becomes the new name (which may equal the path param for a no-op rename).
/// `disabled` / `disabled_tools` / `client_secret` are rejected in the body
/// — those have dedicated routes — and the existing entry's values for
/// those fields are preserved across the update.
async fn update_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
    Json(req): Json<EndpointRequest>,
) -> impl IntoResponse {
    let cfg_snapshot = state.config.read().await.clone();
    let exists = cfg_snapshot.endpoints.iter().any(|e| e.name == name);
    if !exists {
        return endpoint_not_found(&name).into_response();
    }
    let new_ep = match validate_endpoint_request(&req, &cfg_snapshot, Some(&name)) {
        Ok(ep) => ep,
        Err(resp) => return *resp,
    };
    if new_ep.name != name {
        if let Some(resp) = name_reserved_by_setup(&state, &new_ep.name).await {
            return *resp;
        }
    }
    if let Err(resp) = apply_endpoint_change(&state, new_ep.clone(), Some(&name)).await {
        return *resp;
    }
    (StatusCode::OK, Json(endpoint_summary_from(&new_ep))).into_response()
}

// ---------------------------------------------------------------------------
// Tool-call event SSE stream
// ---------------------------------------------------------------------------

/// `GET /api/events/tool-calls` — Server-Sent-Events stream of every
/// [`ToolCallEvent`] published on the relay's typed event bus. Each adapter
/// emits a `started` event at `call_tool` entry and a matching
/// `completed` / `failed` event at the end of the round-trip; the desktop
/// overlay subscribes once per session and renders cards from those events.
///
/// Subscribers are independent broadcast receivers, so a slow / disconnected
/// client only impacts itself: on `Lagged` the handler emits a single
/// `event: lagged` SSE comment and continues from the freshest available
/// frame. Tokio broadcast's drop-oldest semantics guarantee producers
/// (`call_tool` invocations) never block on overlay clients.
///
/// A 15 s SSE keep-alive prevents idle reverse proxies (or, more relevantly,
/// the desktop's Unix-socket HTTP client) from collapsing the connection.
async fn tool_call_events_sse(State(state): State<ManagementState>) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::convert::Infallible;
    use std::time::Duration;
    use tokio::sync::broadcast::error::RecvError;
    use tokio_stream::wrappers::ReceiverStream;

    let Some(bus) = state.event_bus.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "event bus not configured").into_response();
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    let mut event_rx = bus.subscribe();

    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(ev) => {
                    let frame = match serde_json::to_string(&ev) {
                        Ok(s) => Event::default().data(s),
                        Err(e) => {
                            warn!(error = %e, "Failed to serialize ToolCallEvent for SSE stream");
                            continue;
                        }
                    };
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    let frame = Event::default()
                        .event("lagged")
                        .data(format!("{{\"skipped\":{}}}", skipped));
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

// ---------------------------------------------------------------------------
// Observability API (R5)
// ---------------------------------------------------------------------------

/// Serializable view of a [`CallRecord`] for the observability API. Mirrors the
/// stored metadata row; request/response bodies are surfaced separately on the
/// drill-through route, never on the list.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallRecordDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_origin: Option<String>,
    tool: String,
    ts_start: i64,
    ts_end: i64,
    duration_ms: i64,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    request_bytes: i64,
    response_bytes: i64,
    streamed: bool,
}

impl From<CallRecord> for CallRecordDto {
    fn from(r: CallRecord) -> Self {
        CallRecordDto {
            id: r.id,
            request_uid: r.request_uid,
            endpoint: r.endpoint,
            server_name: r.server_name,
            server_type: r.server_type,
            transport: r.transport,
            profile: r.profile,
            client_name: r.client_name,
            client_version: r.client_version,
            client_user_agent: r.client_user_agent,
            client_origin: r.client_origin,
            tool: r.tool,
            ts_start: r.ts_start,
            ts_end: r.ts_end,
            duration_ms: r.duration_ms,
            success: r.success,
            error_message: r.error_message,
            request_bytes: r.request_bytes,
            response_bytes: r.response_bytes,
            streamed: r.streamed,
        }
    }
}

/// Slim per-row DTO for the calls list. Drops the verbose
/// transport/client/payload-byte detail columns the table doesn't render so a
/// 100-row page stays a few KB instead of tens of KB on the wire. The full
/// row (`CallRecordDto`) is still served by the drill-through detail route.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallSummaryDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_name: Option<String>,
    tool: String,
    ts_start: i64,
    duration_ms: i64,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    request_bytes: i64,
    response_bytes: i64,
}

impl From<CallRecord> for CallSummaryDto {
    fn from(r: CallRecord) -> Self {
        CallSummaryDto {
            id: r.id,
            request_uid: r.request_uid,
            server_name: r.server_name,
            tool: r.tool,
            ts_start: r.ts_start,
            duration_ms: r.duration_ms,
            success: r.success,
            error_message: r.error_message,
            request_bytes: r.request_bytes,
            response_bytes: r.response_bytes,
        }
    }
}

/// Query string for `GET /api/observability/calls`. All filters are optional
/// and ANDed; `since`/`until` bound `ts_start` as epoch milliseconds. `cursor`
/// is an opaque base64url token returned as `nextCursor` on the previous page;
/// omit it to fetch the first page.
#[derive(Deserialize)]
struct CallsQuery {
    server_name: Option<String>,
    tool: Option<String>,
    success: Option<bool>,
    request_uid: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    limit: Option<i64>,
    cursor: Option<String>,
}

/// Response for `GET /api/observability/calls`. `next_cursor` is the opaque
/// token to pass back as `cursor` for the next page; absent when the page is
/// the last one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallsResponse {
    calls: Vec<CallSummaryDto>,
    limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

/// Default and maximum page sizes for the calls list.
const CALLS_DEFAULT_LIMIT: i64 = 100;
const CALLS_MAX_LIMIT: i64 = 1000;

/// Encode a `(ts_start, id)` keyset cursor as a stable, opaque base64url token.
/// The format is intentionally undocumented to the client — it's a substring
/// of the response, not a contract.
fn encode_calls_cursor(ts_start: i64, id: i64) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(format!("{ts_start}:{id}"))
}

/// Decode a `(ts_start, id)` keyset cursor. Returns `None` for any malformed
/// input so the caller can reject it with a 400.
fn decode_calls_cursor(token: &str) -> Option<(i64, i64)> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
    let s = std::str::from_utf8(&bytes).ok()?;
    let (ts, id) = s.split_once(':')?;
    Some((ts.parse().ok()?, id.parse().ok()?))
}

/// 503 body used when the observability subsystem is unwired (e.g. the metadata
/// store failed to open at startup). Distinct from a wired-but-disabled handle:
/// a disabled handle still answers reads (its store is empty / frozen).
fn observability_unavailable() -> axum::response::Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "observability not available",
        Some("The observability subsystem is not initialised."),
    )
    .into_response()
}

/// Current wall-clock time in epoch milliseconds (default `until` bound).
fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// GET /api/observability/calls — filtered, paged metadata list (payloads
/// excluded; use the drill-through route for those). Pagination is keyset on
/// `(ts_start, id)` via the opaque `cursor` query param so paging stays
/// stable across concurrent inserts and is O(limit) at any depth.
async fn get_observability_calls(
    State(state): State<ManagementState>,
    Query(q): Query<CallsQuery>,
) -> axum::response::Response {
    let Some(obs) = state.registry.observability() else {
        return observability_unavailable();
    };
    let limit = q
        .limit
        .unwrap_or(CALLS_DEFAULT_LIMIT)
        .clamp(1, CALLS_MAX_LIMIT);
    let cursor = match q.cursor.as_deref() {
        Some(token) => match decode_calls_cursor(token) {
            Some(c) => Some(c),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid cursor",
                    Some("The cursor query parameter is not a valid continuation token."),
                )
                .into_response();
            }
        },
        None => None,
    };
    let filter = QueryFilter {
        server_name: q.server_name,
        tool: q.tool,
        success: q.success,
        request_uid: q.request_uid,
        since: q.since,
        until: q.until,
    };
    // Offload the synchronous rusqlite query to a blocking thread so a slow
    // query never stalls the async runtime serving management + relay tasks.
    let store = Arc::clone(obs.store());
    let queried = tokio::task::spawn_blocking(move || store.query(&filter, limit, cursor)).await;
    match queried {
        Ok(Ok(rows)) => {
            // Emit a continuation token only when the page filled the limit —
            // otherwise we're at the tail and there's no more to fetch. The
            // token encodes the last row's `(ts_start, id)`, which the next
            // request feeds back via `?cursor=`.
            let next_cursor = if rows.len() as i64 >= limit {
                rows.last()
                    .and_then(|r| r.id.map(|id| encode_calls_cursor(r.ts_start, id)))
            } else {
                None
            };
            Json(CallsResponse {
                calls: rows.into_iter().map(CallSummaryDto::from).collect(),
                limit,
                next_cursor,
            })
            .into_response()
        }
        Ok(Err(e)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to query observability records",
            Some(&e.to_string()),
        )
        .into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to query observability records",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

/// Payload availability on the drill-through route.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum PayloadStatus {
    /// Payloads are still buffered and returned inline.
    Stored,
    /// Payload capture is enabled but the entry aged out of the ring buffer.
    Expired,
    /// Payload capture is disabled (`store_payloads = false`).
    Disabled,
}

/// Response for `GET /api/observability/calls/{request_uid}`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallDetailResponse {
    record: CallRecordDto,
    payload_status: PayloadStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    payloads: Option<StoredPayloads>,
}

/// GET /api/observability/calls/{request_uid} — full metadata row plus buffered
/// payloads when still retained.
async fn get_observability_call_detail(
    State(state): State<ManagementState>,
    Path(request_uid): Path<String>,
) -> axum::response::Response {
    let Some(obs) = state.registry.observability() else {
        return observability_unavailable();
    };
    // Offload the synchronous rusqlite lookup to a blocking thread so a slow
    // query never stalls the async runtime serving management + relay tasks.
    let store = Arc::clone(obs.store());
    let uid = request_uid.clone();
    let looked_up = tokio::task::spawn_blocking(move || store.get_by_request_uid(&uid)).await;
    let record = match looked_up {
        Ok(Ok(Some(r))) => r,
        Ok(Ok(None)) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "call record not found",
                Some(&format!("No record for request_uid {request_uid}")),
            )
            .into_response();
        }
        Ok(Err(e)) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load observability record",
                Some(&e.to_string()),
            )
            .into_response();
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load observability record",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };
    let (payload_status, payloads) = if !obs.store_payloads() {
        (PayloadStatus::Disabled, None)
    } else {
        match obs.payloads().get(&request_uid) {
            Some(p) => (PayloadStatus::Stored, Some(p)),
            None => (PayloadStatus::Expired, None),
        }
    };
    Json(CallDetailResponse {
        record: record.into(),
        payload_status,
        payloads,
    })
    .into_response()
}

/// Serializable view of an [`AggregateBucket`] sparkline point.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AggregateBucketDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    bucket_start: i64,
    count: u64,
    error_count: u64,
    p50_ms: u64,
    p95_ms: u64,
}

impl From<AggregateBucket> for AggregateBucketDto {
    fn from(b: AggregateBucket) -> Self {
        AggregateBucketDto {
            server: b.server,
            bucket_start: b.bucket_start,
            count: b.count,
            error_count: b.error_count,
            p50_ms: b.p50_ms,
            p95_ms: b.p95_ms,
        }
    }
}

/// Global counters surfaced alongside the sparkline buckets so the desktop can
/// render overflow and buffer-pressure indicators.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservabilitySummary {
    enabled: bool,
    store_payloads: bool,
    dropped: u64,
    payload_buffer_len: usize,
    payload_buffer_bytes: usize,
}

/// Query string for `GET /api/observability/aggregates`.
#[derive(Deserialize)]
struct AggregatesQuery {
    bucket_seconds: Option<i64>,
    since: Option<i64>,
    until: Option<i64>,
}

/// Response for `GET /api/observability/aggregates`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AggregatesResponse {
    buckets: Vec<AggregateBucketDto>,
    summary: ObservabilitySummary,
}

/// Default sparkline window (1 hour) and bucket width (1 minute).
const AGGREGATES_DEFAULT_WINDOW_MS: i64 = 3_600_000;
const AGGREGATES_DEFAULT_BUCKET_SECONDS: i64 = 60;

/// GET /api/observability/aggregates — time-bucketed per-server + global
/// metrics for the SVG sparklines, plus global overflow / buffer counters.
async fn get_observability_aggregates(
    State(state): State<ManagementState>,
    Query(q): Query<AggregatesQuery>,
) -> axum::response::Response {
    let Some(obs) = state.registry.observability() else {
        return observability_unavailable();
    };
    let until = q.until.unwrap_or_else(now_epoch_ms);
    let since = q.since.unwrap_or(until - AGGREGATES_DEFAULT_WINDOW_MS);
    let bucket_seconds = q
        .bucket_seconds
        .unwrap_or(AGGREGATES_DEFAULT_BUCKET_SECONDS)
        .max(1);
    let summary = ObservabilitySummary {
        enabled: obs.is_enabled(),
        store_payloads: obs.store_payloads(),
        dropped: obs.dropped(),
        payload_buffer_len: obs.payloads().len(),
        payload_buffer_bytes: obs.payloads().total_bytes(),
    };
    // Offload the synchronous rusqlite aggregation to a blocking thread so a
    // slow query never stalls the async runtime serving management + relay tasks.
    let store = Arc::clone(obs.store());
    let aggregated =
        tokio::task::spawn_blocking(move || store.aggregate(bucket_seconds, since, until)).await;
    match aggregated {
        Ok(Ok(buckets)) => Json(AggregatesResponse {
            buckets: buckets.into_iter().map(AggregateBucketDto::from).collect(),
            summary,
        })
        .into_response(),
        Ok(Err(e)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to aggregate observability records",
            Some(&e.to_string()),
        )
        .into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to aggregate observability records",
            Some(&e.to_string()),
        )
        .into_response(),
    }
}

/// POST /api/observability/purge — drop every buffered payload and metadata row.
async fn purge_observability(State(state): State<ManagementState>) -> axum::response::Response {
    let Some(obs) = state.registry.observability() else {
        return observability_unavailable();
    };
    // Offload the synchronous rusqlite purge to a blocking thread so it never
    // stalls the async runtime serving management + relay tasks.
    let store = Arc::clone(obs.store());
    let purged = tokio::task::spawn_blocking(move || store.purge_all()).await;
    match purged {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to purge observability metadata",
                Some(&e.to_string()),
            )
            .into_response();
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to purge observability metadata",
                Some(&e.to_string()),
            )
            .into_response();
        }
    }
    obs.payloads().purge_all();
    Json(ActionResponse {
        ok: true,
        message: "observability records purged".to_string(),
    })
    .into_response()
}

/// GET /api/observability/config — current `[relay.observability]` settings.
async fn get_observability_config(
    State(state): State<ManagementState>,
) -> Json<ObservabilityConfig> {
    let config = state.config.read().await;
    Json(config.relay.observability.clone())
}

/// PUT /api/observability/config — persist new `[relay.observability]` settings
/// to disk and swap the in-memory baseline. Like `persist_disabled_state`, this
/// reparses the whole config file into a `toml::Table`, replaces only the
/// `[relay.observability]` table, and reserializes the entire file — unknown
/// sections and keys survive, though comments/formatting are not preserved and
/// sections may be reordered. Runtime store sizing (windows, budgets,
/// enable/disable) is re-read on the next relay restart.
async fn update_observability_config(
    State(state): State<ManagementState>,
    Json(new_cfg): Json<ObservabilityConfig>,
) -> axum::response::Response {
    let Some(config_path) = &state.config_path else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_path not configured",
            Some("The management API was not initialised with a config file path."),
        )
        .into_response();
    };
    let resolved = crate::config::expand_tilde(config_path);

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

    let obs_value = match toml::Value::try_from(&new_cfg) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to serialize observability config",
                Some(&e.to_string()),
            )
            .into_response();
        }
    };

    // Insert/replace `[relay.observability]`, creating `[relay]` if absent.
    let relay_tbl = parsed
        .entry("relay".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(relay_tbl) = relay_tbl.as_table_mut() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid config file",
            Some("`relay` is not a table"),
        )
        .into_response();
    };
    relay_tbl.insert("observability".to_string(), obs_value);

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
    if let Err(e) = crate::config::write_config_file(&resolved, &new_contents) {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write config file",
            Some(&e.to_string()),
        )
        .into_response();
    }

    // Swap the in-memory baseline so `GET /api/config` and
    // `GET /api/observability/config` reflect the change immediately.
    {
        let mut config = state.config.write().await;
        config.relay.observability = new_cfg.clone();
    }

    Json(new_cfg).into_response()
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the management API router with all /api routes.
pub fn management_routes(state: ManagementState) -> Router {
    warm_container_runtime_detection();
    Router::new()
        .route("/api/status", get(get_status))
        .route("/api/endpoints", get(get_endpoints).post(create_endpoint))
        .route(
            "/api/endpoints/{name}",
            delete(delete_endpoint).put(update_endpoint),
        )
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
        .route(
            "/api/endpoints/{name}/credentials",
            get(get_endpoint_credentials).post(set_endpoint_credentials),
        )
        .route("/api/endpoints/{name}/oauth/start", post(oauth_start))
        .route(
            "/api/endpoints/{name}/oauth/credentials",
            post(oauth_credentials),
        )
        .route("/api/endpoints/{name}/oauth/status", get(oauth_status))
        .route("/api/endpoints/{name}/oauth/revoke", post(oauth_revoke))
        .route("/api/endpoints/{name}/oauth/reset", post(oauth_reset))
        .route("/api/endpoints/{name}/oauth/refresh", post(oauth_refresh))
        .route("/api/endpoints/{name}/oauth/metrics", get(oauth_metrics))
        // OAuth capability probe (add-time)
        .route("/api/oauth/probe", post(oauth_probe))
        // OAuth setup (preflight) routes
        .route("/api/oauth/setup", post(oauth_setup))
        .route(
            "/api/oauth/setup/{id}/credentials",
            post(oauth_setup_credentials),
        )
        .route("/api/oauth/setup/{id}/status", get(oauth_setup_status))
        .route("/api/oauth/setup/{id}/commit", post(oauth_setup_commit))
        .route("/api/oauth/setup/{id}", delete(oauth_setup_cancel))
        .route("/api/catalog", get(get_catalog))
        .route("/api/config", get(get_config))
        .route("/api/config/reload", post(reload_config))
        .route("/api/test-connection", post(test_connection))
        // Profile CRUD (R4.A) — spec §8.1, §8.2.
        .route("/api/profiles", get(list_profiles).post(create_profile))
        .route(
            "/api/profiles/{path}",
            get(get_profile).put(update_profile).delete(delete_profile),
        )
        // Endpoint → profile membership (read-only, spec §8.3).
        .route(
            "/api/endpoints/{name}/profiles",
            get(get_endpoint_profile_membership),
        )
        // Desktop overlay's typed tool-call event SSE stream.
        .route("/api/events/tool-calls", get(tool_call_events_sse))
        // Observability tab (R5): query, drill-through, aggregates, purge,
        // config, and a live SSE feed reusing the tool-call event bus.
        .route("/api/observability/calls", get(get_observability_calls))
        .route(
            "/api/observability/calls/{request_uid}",
            get(get_observability_call_detail),
        )
        .route(
            "/api/observability/aggregates",
            get(get_observability_aggregates),
        )
        .route("/api/observability/purge", post(purge_observability))
        .route(
            "/api/observability/config",
            get(get_observability_config).put(update_observability_config),
        )
        .route("/api/observability/events", get(tool_call_events_sse))
        // END-19 Wave 3: provider templates + organization lifecycle.
        .route("/api/idp-providers", get(list_idp_providers))
        .route(
            "/api/organizations",
            get(list_organizations).post(create_organization),
        )
        .route(
            "/api/organizations/{org}",
            delete(delete_organization).put(update_organization),
        )
        .route(
            "/api/organizations/{org}/reauthenticate",
            post(reauthenticate_organization),
        )
        // EMA capability probe: which desktop-supplied MCP servers can this org reach?
        .route("/api/organizations/{org}/probe", post(probe_organization))
        // No CORS layer: this router is served exclusively over a Unix-domain
        // socket / Windows named pipe (see `management_listener`), which is not
        // reachable from a browser and has no cross-origin attack surface.
        .with_state(state)
}

// ---------------------------------------------------------------------------
// END-19 Wave 3: provider templates + organization lifecycle
// ---------------------------------------------------------------------------

/// Request body for `POST /api/organizations`.
#[derive(Deserialize)]
struct CreateOrganizationRequest {
    /// Display name / stable key (e.g. "Acme Corp"); also the credential-pool key.
    name: String,
    /// Provider template id: `okta`, `entra`, `google`, `ping`, or `custom`.
    provider: String,
    /// Slug for templated providers (Okta subdomain, Entra tenant, Ping env id).
    #[serde(default)]
    slug: Option<String>,
    /// Full issuer URL for `provider = "custom"` (pasted by the user).
    #[serde(default)]
    idp: Option<String>,
    /// Optional pre-registered OAuth `client_id` for this org's IdP (e.g. an
    /// Okta/Entra app registration). When provided it wins the resolution chain
    /// and is persisted on the org; when omitted the relay falls back to CIMD/DCR.
    #[serde(default)]
    client_id: Option<String>,
    /// Optional pre-registered OAuth `client_secret` for confidential IdP clients
    /// that require the authorization-code → token exchange (and later EMA legs)
    /// to authenticate with `client_secret_post`. Persisted in the secure
    /// credential store keyed by org name (`{org}.dcr.json`, 0600); never
    /// written to `config.toml` and never returned to the UI.
    #[serde(default)]
    client_secret: Option<String>,
}

/// Request body for `PUT /api/organizations/{org}`.
///
/// Every field is optional — omitted fields preserve the current value.
/// `client_id` and `client_secret` use an empty string (`""`) as the explicit
/// "clear" signal so callers can distinguish "keep" (absent) from "remove"
/// (present-and-empty) without a serde absent-vs-null dance.
#[derive(Deserialize)]
struct UpdateOrganizationRequest {
    /// New display name. When supplied and different from the path segment the
    /// org is renamed (and pooled IdP credentials are purged — see handler).
    #[serde(default)]
    name: Option<String>,
    /// New provider template id (`okta`, `entra`, `google`, `ping`, `custom`).
    #[serde(default)]
    provider: Option<String>,
    /// New slug for templated providers (Okta subdomain, Entra tenant, …).
    #[serde(default)]
    slug: Option<String>,
    /// New full issuer URL for `provider = "custom"`.
    #[serde(default)]
    idp: Option<String>,
    /// New explicit `client_id`. Empty string clears the persisted id so the
    /// next resolution falls back to CIMD/DCR. Identity-affecting — see handler.
    #[serde(default)]
    client_id: Option<String>,
    /// New confidential-client `client_secret`. Empty string deletes the
    /// stored secret (org returns to public/PKCE behaviour). Never written to
    /// `config.toml`; persisted at `{org}.dcr.json` (0600).
    #[serde(default)]
    client_secret: Option<String>,
}

/// One organization entry returned by `GET /api/organizations`.
#[derive(Serialize)]
struct OrganizationResponse {
    name: String,
    provider: String,
    idp: String,
    /// Whether the credential pool holds usable IdP credentials for this org
    /// (a non-expired ID token or a refresh token to silently re-mint one).
    authenticated: bool,
}

/// Response carrying a freshly-composed IdP SSO authorize URL.
#[derive(Serialize)]
struct OrganizationSsoResponse {
    name: String,
    provider: String,
    idp: String,
    authorize_url: String,
}

/// GET /api/idp-providers
///
/// Returns the static provider template table — the single source of truth the
/// desktop "Add organization" UI renders and `POST /api/organizations` resolves
/// issuer URLs from.
async fn list_idp_providers() -> impl IntoResponse {
    Json(crate::oauth::idp_providers::IDP_PROVIDERS)
}

/// Validate an IdP issuer via RFC 8414 / OIDC discovery before persisting an
/// organization. Returns the discovered metadata (used to compose the SSO URL)
/// or a ready-to-return `400` response. The SSRF guard inside discovery rejects
/// internal/loopback hosts unless `allow_insecure` is set.
#[allow(clippy::result_large_err)] // Err is a ready-to-return axum Response
async fn validate_org_issuer(
    issuer: &str,
    allow_insecure: bool,
) -> Result<crate::oauth::discovery::DiscoveryResult, axum::response::Response> {
    crate::oauth::discovery::discover_authorization_server(issuer, allow_insecure)
        .await
        .map_err(|e| {
            error_response(
                StatusCode::BAD_REQUEST,
                "invalid_issuer",
                Some(&format!("Could not validate IdP issuer '{issuer}': {e}")),
            )
            .into_response()
        })
}

/// Resolve the OAuth `client_id` for an organization's IdP via the shared
/// fallback chain (explicit org `client_id` → CIMD when the AS advertises it →
/// DCR when a registration endpoint exists), returning the resolved id and which
/// path produced it. A `422 client_id_required` response is returned when the
/// IdP supports neither CIMD nor DCR and no explicit `client_id` was supplied.
#[allow(clippy::result_large_err)] // Err is a ready-to-return axum Response
async fn resolve_org_client(
    org_client_id: Option<&str>,
    disc: &crate::oauth::discovery::DiscoveryResult,
    redirect_uri: &str,
    org_name: &str,
    allow_insecure: bool,
) -> Result<(String, ClientRegistration), axum::response::Response> {
    let explicit = org_client_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| (s.to_string(), None));
    let dcr_redirect_uri = redirect_uri.to_string();
    let dcr_org_name = org_name.to_string();
    let resolved = client::resolve_client(
        client::ClientInputs {
            explicit_manual: explicit,
            preregistered: None,
            cimd_supported: disc.client_id_metadata_document_supported,
            registration_endpoint: disc.registration_endpoint.clone(),
        },
        |reg_endpoint| async move {
            let resp = crate::oauth::dcr::register_client(
                &reg_endpoint,
                &dcr_redirect_uri,
                &dcr_org_name,
                allow_insecure,
            )
            .await?;
            Ok::<_, crate::oauth::dcr::DcrError>(client::DcrOutcome {
                client_id: resp.client_id,
                client_secret: resp.client_secret,
                client_secret_expires_at: resp.client_secret_expires_at,
            })
        },
    )
    .await;

    match resolved {
        Ok(r) => Ok((r.client_id, r.registration)),
        Err(client::ClientResolveError::NoCredentials) => Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "client_id_required",
            Some(
                "This IdP advertises neither CIMD nor Dynamic Client Registration; \
                 provide a pre-registered 'client_id' for the organization.",
            ),
        )
        .into_response()),
        Err(client::ClientResolveError::Dcr(e)) => Err(error_response(
            StatusCode::BAD_GATEWAY,
            "dcr_failed",
            Some(&format!("Dynamic Client Registration failed: {e}")),
        )
        .into_response()),
    }
}

/// Compose the IdP SSO authorize URL for an organization and register the
/// pending IdP flow via [`OAuthFlowManager::start_idp_flow`], mirroring the EMA
/// adapter's `compose_idp_authorize_url`. The captured ID token is keyed by the
/// org `name` so every EMA endpoint in the org shares one credential. The
/// requested scope is `openid offline_access` (M1) so the IdP returns a refresh
/// token. The configured `issuer` (not the discovered canonical form) is stored
/// in the flow so the credentials match the issuer the EMA chain re-discovers.
/// The `client_id` is the value resolved by [`resolve_org_client`] (explicit /
/// CIMD / DCR) and is presented identically in the URL and the pending flow.
async fn compose_org_sso_url(
    flow_mgr: &OAuthFlowManager,
    relay_port: u16,
    org_name: &str,
    issuer: &str,
    disc: &crate::oauth::discovery::DiscoveryResult,
    client_id: &str,
    client_secret: Option<&str>,
) -> String {
    let pkce = PkceChallenge::generate();
    let code_challenge = pkce.code_challenge.clone();
    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", relay_port);

    let state_param = flow_mgr
        .start_idp_flow(
            org_name,
            &disc.token_endpoint,
            client_id,
            client_secret,
            pkce,
            &redirect_uri,
            Some(issuer),
            false,
            issuer,
            org_name,
        )
        .await;

    let sep = if disc.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut authorize_url = format!(
        "{}{}response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}",
        disc.authorization_endpoint,
        sep,
        urlencoding(client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state_param),
        urlencoding(&code_challenge),
        urlencoding("openid offline_access"),
    );
    // Google needs `access_type=offline` for a refresh token (shared helper);
    // a Google-fronted org SSO grant is access-token-only without it.
    append_google_authorize_params(&mut authorize_url, &disc.authorization_endpoint);
    authorize_url
}

/// Persist the updated organization list back to `config.toml`, preserving the
/// rest of the document. Mirrors [`write_profiles_to_disk`]. Tokens are never
/// written here — only `name`/`provider`/`idp` round-trip through `config.toml`.
fn write_organizations_to_disk(
    config_path: &std::path::Path,
    organizations: &[crate::config::ConfigOrganization],
) -> Result<(), (StatusCode, &'static str, String)> {
    let contents = std::fs::read_to_string(config_path).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read config file",
            e.to_string(),
        )
    })?;
    let mut parsed: toml::Table = contents.parse().map_err(|e: toml::de::Error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to parse config file",
            e.to_string(),
        )
    })?;

    if organizations.is_empty() {
        parsed.remove("organizations");
    } else {
        let arr: Vec<toml::Value> = organizations
            .iter()
            .map(|o| {
                toml::Value::try_from(o)
                    .expect("ConfigOrganization is Serialize and round-trips through toml::Value")
            })
            .collect();
        parsed.insert("organizations".into(), toml::Value::Array(arr));
    }

    let new_contents = toml::to_string_pretty(&parsed).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to serialize config",
            e.to_string(),
        )
    })?;
    crate::config::write_config_file(config_path, &new_contents).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to write config file",
            e.to_string(),
        )
    })?;
    Ok(())
}

/// POST /api/organizations
///
/// Builds the IdP issuer from a provider template (`{provider, slug}`) or takes
/// a pasted custom issuer (`{provider: "custom", idp}`), validates it via
/// discovery **before** persisting, writes the org to `config.toml`, and returns
/// an SSO authorize URL (reusing `start_idp_flow` + `/oauth/callback`).
async fn create_organization(
    State(state): State<ManagementState>,
    Json(body): Json<CreateOrganizationRequest>,
) -> impl IntoResponse {
    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            None,
        )
        .into_response();
    };
    let Some(ref config_path) = state.config_path else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_path not configured",
            None,
        )
        .into_response();
    };

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_name",
            Some("Organization name must not be empty."),
        )
        .into_response();
    }

    // Snapshot the SSRF opt-out and reject duplicate org names.
    let allow_insecure = {
        let config = state.config.read().await;
        if config.organizations.iter().any(|o| o.name == name) {
            return error_response(
                StatusCode::CONFLICT,
                "organization_exists",
                Some(&format!(
                    "An organization named '{name}' already exists. Use a different name."
                )),
            )
            .into_response();
        }
        config.relay.allow_insecure_oauth.unwrap_or(false)
    };

    // Resolve the issuer: a pasted custom URL, or built from a provider template.
    let issuer = if body.provider == "custom" {
        match body
            .idp
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            Some(idp) => idp.to_string(),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "missing_idp",
                    Some("provider 'custom' requires a full 'idp' issuer URL."),
                )
                .into_response();
            }
        }
    } else {
        let Some(provider) = crate::oauth::idp_providers::find_provider(&body.provider) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "unknown_provider",
                Some(&format!(
                    "Unknown provider '{}'. Use GET /api/idp-providers to list templates.",
                    body.provider
                )),
            )
            .into_response();
        };
        match provider.build_issuer(body.slug.as_deref()) {
            Ok(issuer) => issuer,
            Err(e) => {
                return error_response(StatusCode::BAD_REQUEST, "invalid_slug", Some(&e))
                    .into_response();
            }
        }
    };

    // Validate the issuer via discovery BEFORE persisting (DoD: bad issuer
    // rejected pre-save; custom paste validated via discovery).
    let disc = match validate_org_issuer(&issuer, allow_insecure).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    // Resolve the org's OAuth client_id via the shared fallback chain BEFORE
    // persisting (explicit org client_id → CIMD → DCR → 422). The resolved id is
    // used verbatim for the authorize URL and every later EMA token leg.
    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);
    let (resolved_client_id, registration) = match resolve_org_client(
        body.client_id.as_deref(),
        &disc,
        &redirect_uri,
        &name,
        allow_insecure,
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // CIMD resolves to the hosted CIMD URL (the runtime default), so persist
    // `None` to keep the org config round-tripping unchanged and the legs on
    // their byte-for-byte CIMD behavior. An explicit or DCR-registered id is
    // persisted so every leg reuses the same client_id.
    let persisted_client_id = match registration {
        ClientRegistration::Cimd => None,
        _ => Some(resolved_client_id.clone()),
    };

    // Persist the org to config.toml, then mirror into the in-memory config.
    let new_org = crate::config::ConfigOrganization {
        name: name.clone(),
        provider: body.provider.clone(),
        idp: issuer.clone(),
        client_id: persisted_client_id,
    };
    let mut orgs = { state.config.read().await.organizations.clone() };
    orgs.push(new_org.clone());
    let resolved = crate::config::expand_tilde(config_path);
    if let Err((status, msg, detail)) = write_organizations_to_disk(&resolved, &orgs) {
        return error_response(status, msg, Some(&detail)).into_response();
    }
    state.config.write().await.organizations = orgs;

    // Confidential-client support: persist an optional requesting `client_secret`
    // in the secure credential store keyed by org name (`{org}.dcr.json`, 0600).
    // It is never written to `config.toml` or returned to the UI. When omitted,
    // no DCR file is written and the public/PKCE behaviour is preserved
    // byte-for-byte. The stored `client_id` is the resolved id used for the
    // authorize URL so the auth-code exchange (and later EMA legs) present a
    // consistent (client_id, client_secret) pair. The EMA **resource** credential
    // pair is per-resource and lives on the endpoint DCR record (R3), captured via
    // POST /api/endpoints/{name}/credentials, never on the org record.
    let trimmed_secret = body
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(tm) = state.token_manager.as_ref() {
        if trimmed_secret.is_some() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let creds = DcrCredentials {
                client_id: resolved_client_id.clone(),
                client_secret: trimmed_secret.map(str::to_string),
                client_secret_expires_at: 0,
                registered_at: now,
                issuer: Some(issuer.clone()),
                // Org SSO secret is user-supplied via the create route.
                registered_via_dcr: false,
                ..Default::default()
            };
            if let Err(e) = tm.save_dcr(&name, &creds).await {
                warn!(organization = %name, error = %e, "Failed to persist org credentials to DCR store");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to persist client secret",
                    Some(&e.to_string()),
                )
                .into_response();
            }
        }
    }

    let authorize_url = compose_org_sso_url(
        flow_mgr,
        state.relay_port,
        &name,
        &issuer,
        &disc,
        &resolved_client_id,
        trimmed_secret,
    )
    .await;

    info!(organization = %name, provider = %body.provider, "Organization created; SSO authorize URL composed");

    (
        StatusCode::CREATED,
        Json(OrganizationSsoResponse {
            name,
            provider: body.provider,
            idp: issuer,
            authorize_url,
        }),
    )
        .into_response()
}

/// PUT /api/organizations/{org}
///
/// Updates an existing organization's `name`, `provider`/`slug`/`idp`,
/// `client_id`, and/or `client_secret`. Body fields are all optional —
/// omitted fields preserve the current value; `client_id` / `client_secret`
/// use an empty string to mean "clear" (see [`UpdateOrganizationRequest`]).
///
/// **Credential invalidation strategy (chosen: purge + require re-auth).**
/// Identity-affecting changes — rename, issuer change (via provider/slug/idp),
/// or `client_id` change — purge the pooled IdP credentials at both the old
/// and new keys via [`TokenManager::delete_idp`] so the next use forces a
/// fresh SSO. A rename also purges the cached DCR at the old name (the file
/// is keyed by org name); the user must re-supply `client_secret` in this
/// same call (or a follow-up call) to restore confidential-client behaviour.
/// The "purge + require re-auth" choice is explicitly allowed by the slice
/// spec (rename can either rekey OR purge — purge is simpler and consistent
/// with how issuer/client_id changes invalidate the auth state already).
///
/// `client_secret` round-trips through the secure store (`{org}.dcr.json`,
/// 0600) and is never written to `config.toml`; an empty-string secret
/// deletes the stored entry.
///
/// **Returns:** when an identity-affecting change occurred the response
/// mirrors [`OrganizationSsoResponse`] (the caller must re-run SSO with the
/// returned `authorize_url`); otherwise it mirrors [`OrganizationResponse`]
/// with the refreshed metadata + current auth status. Unknown org → 404.
async fn update_organization(
    State(state): State<ManagementState>,
    Path(org): Path<String>,
    Json(body): Json<UpdateOrganizationRequest>,
) -> impl IntoResponse {
    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            None,
        )
        .into_response();
    };
    let Some(ref config_path) = state.config_path else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_path not configured",
            None,
        )
        .into_response();
    };

    // Look up the existing org; capture snapshot for diffing.
    let (current, allow_insecure, orgs_before) = {
        let config = state.config.read().await;
        let Some(found) = config.organizations.iter().find(|o| o.name == org).cloned() else {
            return error_response(
                StatusCode::NOT_FOUND,
                "organization not found",
                Some(&format!("No organization named '{org}'.")),
            )
            .into_response();
        };
        (
            found,
            config.relay.allow_insecure_oauth.unwrap_or(false),
            config.organizations.clone(),
        )
    };

    // Resolve effective field values: present-and-non-empty → use; absent → keep.
    let new_name = body
        .name
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| current.name.clone());
    let new_provider = body
        .provider
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| current.provider.clone());

    // Reject duplicate name on rename.
    if new_name != current.name && orgs_before.iter().any(|o| o.name == new_name) {
        return error_response(
            StatusCode::CONFLICT,
            "organization_exists",
            Some(&format!(
                "An organization named '{new_name}' already exists. Use a different name."
            )),
        )
        .into_response();
    }

    // Resolve effective issuer: rebuild from provider+slug, take pasted custom,
    // or keep the current org.idp when neither side moved.
    let issuer_in_body = body.slug.is_some() || body.idp.is_some();
    let provider_changed = new_provider != current.provider;
    let new_issuer = if provider_changed || issuer_in_body {
        if new_provider == "custom" {
            match body
                .idp
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                Some(idp) => idp.to_string(),
                None if !provider_changed => current.idp.clone(),
                None => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "missing_idp",
                        Some("provider 'custom' requires a full 'idp' issuer URL."),
                    )
                    .into_response();
                }
            }
        } else {
            let Some(provider) = crate::oauth::idp_providers::find_provider(&new_provider) else {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "unknown_provider",
                    Some(&format!(
                        "Unknown provider '{}'. Use GET /api/idp-providers to list templates.",
                        new_provider
                    )),
                )
                .into_response();
            };
            match provider.build_issuer(body.slug.as_deref()) {
                Ok(issuer) => issuer,
                Err(e) => {
                    return error_response(StatusCode::BAD_REQUEST, "invalid_slug", Some(&e))
                        .into_response();
                }
            }
        }
    } else {
        current.idp.clone()
    };

    // Effective explicit client_id: empty string clears, non-empty sets,
    // absent preserves the persisted org.client_id.
    let effective_explicit_client_id: Option<String> = match body.client_id.as_ref() {
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(s.trim().to_string()),
        None => current.client_id.clone(),
    };

    // Always re-validate the issuer + re-resolve the client (matches create /
    // reauth) so the persisted client_id and any new authorize URL stay
    // coherent with what the IdP currently advertises.
    let disc = match validate_org_issuer(&new_issuer, allow_insecure).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);
    let (resolved_client_id, registration) = match resolve_org_client(
        effective_explicit_client_id.as_deref(),
        &disc,
        &redirect_uri,
        &new_name,
        allow_insecure,
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let persisted_client_id = match registration {
        ClientRegistration::Cimd => None,
        _ => Some(resolved_client_id.clone()),
    };

    let name_changed = new_name != current.name;
    let issuer_changed = new_issuer != current.idp;
    let client_id_changed = persisted_client_id != current.client_id;
    let identity_changed = name_changed || issuer_changed || client_id_changed;

    // Persist the updated org list to config.toml + in-memory.
    let updated_org = crate::config::ConfigOrganization {
        name: new_name.clone(),
        provider: new_provider.clone(),
        idp: new_issuer.clone(),
        client_id: persisted_client_id.clone(),
    };
    let mut orgs = orgs_before.clone();
    for o in orgs.iter_mut() {
        if o.name == current.name {
            *o = updated_org.clone();
            break;
        }
    }
    let resolved = crate::config::expand_tilde(config_path);
    if let Err((status, msg, detail)) = write_organizations_to_disk(&resolved, &orgs) {
        return error_response(status, msg, Some(&detail)).into_response();
    }
    state.config.write().await.organizations = orgs;

    // Credential invalidation. Pooled IdP credentials are keyed by org name
    // and bound to (issuer, client_id), so any rename / issuer / client_id
    // change makes the cached entry meaningless — purge at both old and new
    // names so a stale entry on either side cannot be picked up next call.
    // DCR cached state is keyed by org name and binds (client_id, secret) to
    // an issuer; purge the old-name DCR on rename and the same-name DCR when
    // issuer/client_id moved so a subsequent secret save lands cleanly.
    if let Some(ref tm) = state.token_manager {
        if identity_changed {
            if let Err(e) = tm.delete_idp(&current.name).await {
                warn!(organization = %current.name, error = %e, "Failed to purge old IdP credentials during update");
            }
            if name_changed {
                if let Err(e) = tm.delete_idp(&new_name).await {
                    warn!(organization = %new_name, error = %e, "Failed to purge new IdP credentials during update");
                }
                if let Err(e) = tm.delete_dcr(&current.name).await {
                    warn!(organization = %current.name, error = %e, "Failed to purge old DCR record during update");
                }
            }
            if !name_changed && (issuer_changed || client_id_changed) {
                if let Err(e) = tm.delete_dcr(&new_name).await {
                    warn!(organization = %new_name, error = %e, "Failed to purge stale DCR record during update");
                }
            }
        }
    }

    // Apply an explicit requesting `client_secret` override to the single
    // `{org}.dcr.json` record: absent in the body preserves the stored value,
    // an empty string clears it, a non-empty value sets it. When cleared the
    // DCR file is deleted (back to public/PKCE). The EMA **resource** credential
    // pair is per-resource and lives on the endpoint DCR record (R3), captured
    // via POST /api/endpoints/{name}/credentials, never on the org record.
    if let Some(action) = body.client_secret.as_deref().map(str::trim) {
        if let Some(tm) = state.token_manager.as_ref() {
            let client_secret = match action {
                "" => None,
                s => Some(s.to_string()),
            };
            if client_secret.is_none() {
                if let Err(e) = tm.delete_dcr(&new_name).await {
                    warn!(organization = %new_name, error = %e, "Failed to clear org credentials from DCR store");
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to clear client secret",
                        Some(&e.to_string()),
                    )
                    .into_response();
                }
            } else {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let creds = DcrCredentials {
                    client_id: resolved_client_id.clone(),
                    client_secret,
                    client_secret_expires_at: 0,
                    registered_at: now,
                    issuer: Some(new_issuer.clone()),
                    // Org SSO secret is user-supplied via the update route.
                    registered_via_dcr: false,
                    ..Default::default()
                };
                if let Err(e) = tm.save_dcr(&new_name, &creds).await {
                    warn!(organization = %new_name, error = %e, "Failed to persist updated org credentials to DCR store");
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to persist client secret",
                        Some(&e.to_string()),
                    )
                    .into_response();
                }
            }
        }
    }

    // Build the response. Identity changes require fresh SSO → return an
    // authorize URL just like create / reauthenticate. Otherwise return the
    // refreshed org metadata with the live auth status.
    if identity_changed {
        // Load any freshly-stored secret so the pending flow carries it.
        let client_secret = match state.token_manager.as_ref() {
            Some(tm) => match tm.load_dcr(&new_name).await {
                Ok(Some(creds)) if creds.client_id == resolved_client_id => creds.client_secret,
                _ => None,
            },
            None => None,
        };
        let authorize_url = compose_org_sso_url(
            flow_mgr,
            state.relay_port,
            &new_name,
            &new_issuer,
            &disc,
            &resolved_client_id,
            client_secret.as_deref(),
        )
        .await;
        info!(
            organization = %new_name,
            previous_name = %current.name,
            "Organization updated; identity changed, re-authentication required",
        );
        return Json(OrganizationSsoResponse {
            name: new_name,
            provider: new_provider,
            idp: new_issuer,
            authorize_url,
        })
        .into_response();
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let authenticated = if let Some(ref tm) = state.token_manager {
        match tm.load_idp(&new_name).await {
            Ok(Some(creds)) => {
                let id_valid = creds.id_token_expires_at.map(|e| e > now).unwrap_or(true);
                id_valid || creds.refresh_token.is_some()
            }
            _ => false,
        }
    } else {
        false
    };
    info!(organization = %new_name, "Organization updated; credentials preserved");
    Json(OrganizationResponse {
        name: new_name,
        provider: new_provider,
        idp: new_issuer,
        authenticated,
    })
    .into_response()
}

/// GET /api/organizations
///
/// Lists configured organizations with their authentication status, read from
/// the credential pool (`TokenManager::load_idp`, keyed by org name).
async fn list_organizations(State(state): State<ManagementState>) -> impl IntoResponse {
    let orgs = { state.config.read().await.organizations.clone() };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut out = Vec::with_capacity(orgs.len());
    for org in orgs {
        let authenticated = if let Some(ref tm) = state.token_manager {
            match tm.load_idp(&org.name).await {
                Ok(Some(creds)) => {
                    let id_valid = creds.id_token_expires_at.map(|e| e > now).unwrap_or(true);
                    id_valid || creds.refresh_token.is_some()
                }
                _ => false,
            }
        } else {
            false
        };
        out.push(OrganizationResponse {
            name: org.name,
            provider: org.provider,
            idp: org.idp,
            authenticated,
        });
    }
    Json(out).into_response()
}

/// DELETE /api/organizations/{org}
///
/// Removes the organization from `config.toml` and purges its credential-pool
/// entry (`TokenManager::delete_idp`, keyed by org name).
async fn delete_organization(
    State(state): State<ManagementState>,
    Path(org): Path<String>,
) -> impl IntoResponse {
    let Some(ref config_path) = state.config_path else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_path not configured",
            None,
        )
        .into_response();
    };

    let mut orgs = { state.config.read().await.organizations.clone() };
    let before = orgs.len();
    orgs.retain(|o| o.name != org);
    if orgs.len() == before {
        return error_response(
            StatusCode::NOT_FOUND,
            "organization not found",
            Some(&format!("No organization named '{org}'.")),
        )
        .into_response();
    }

    let resolved = crate::config::expand_tilde(config_path);
    if let Err((status, msg, detail)) = write_organizations_to_disk(&resolved, &orgs) {
        return error_response(status, msg, Some(&detail)).into_response();
    }
    state.config.write().await.organizations = orgs;

    // Purge the pooled IdP credentials (no-op if none were captured).
    if let Some(ref tm) = state.token_manager {
        if let Err(e) = tm.delete_idp(&org).await {
            warn!(organization = %org, error = %e, "Failed to purge IdP credentials on org delete");
        }
    }

    info!(organization = %org, "Organization deleted and credentials purged");
    Json(serde_json::json!({ "ok": true, "name": org })).into_response()
}

/// POST /api/organizations/{org}/reauthenticate
///
/// Re-discovers the org's IdP issuer (endpoints are not stored in config) and
/// returns a fresh SSO authorize URL for re-running the IdP sign-in.
async fn reauthenticate_organization(
    State(state): State<ManagementState>,
    Path(org): Path<String>,
) -> impl IntoResponse {
    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "OAuth not configured",
            None,
        )
        .into_response();
    };

    let (issuer, provider, org_client_id, allow_insecure) = {
        let config = state.config.read().await;
        let Some(found) = config.organizations.iter().find(|o| o.name == org) else {
            return error_response(
                StatusCode::NOT_FOUND,
                "organization not found",
                Some(&format!("No organization named '{org}'.")),
            )
            .into_response();
        };
        (
            found.idp.clone(),
            found.provider.clone(),
            found.client_id.clone(),
            config.relay.allow_insecure_oauth.unwrap_or(false),
        )
    };

    let disc = match validate_org_issuer(&issuer, allow_insecure).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };

    // Resolve the client_id via the same chain so re-auth uses the org's
    // pre-registered/CIMD/DCR client consistently with creation.
    let redirect_uri = format!("http://127.0.0.1:{}/oauth/callback", state.relay_port);
    let (resolved_client_id, _registration) = match resolve_org_client(
        org_client_id.as_deref(),
        &disc,
        &redirect_uri,
        &org,
        allow_insecure,
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Load the optional confidential-client secret from the secure credential
    // store (`{org}.dcr.json`); when present the auth-code exchange in
    // `/oauth/callback` will include `client_secret` in the form body. Missing
    // DCR / load failure / no secret → public/PKCE flow (existing behaviour).
    let client_secret = match state.token_manager.as_ref() {
        Some(tm) => match tm.load_dcr(&org).await {
            Ok(Some(creds)) if creds.client_id == resolved_client_id => creds.client_secret,
            Ok(_) => None,
            Err(e) => {
                warn!(organization = %org, error = %e, "Failed to load org DCR credentials; continuing as public client");
                None
            }
        },
        None => None,
    };

    let authorize_url = compose_org_sso_url(
        flow_mgr,
        state.relay_port,
        &org,
        &issuer,
        &disc,
        &resolved_client_id,
        client_secret.as_deref(),
    )
    .await;

    info!(organization = %org, "Organization re-authentication SSO authorize URL composed");

    Json(OrganizationSsoResponse {
        name: org,
        provider,
        idp: issuer,
        authorize_url,
    })
    .into_response()
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
    match entry.cached_list_tools().await {
        Ok(tools) => {
            let mut tools_with_status: Vec<ToolInfoWithStatus> = tools
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
            tools_with_status.sort_by(|a, b| a.name.cmp(&b.name));
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

    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    Json(catalog)
}

/// POST /api/endpoints/:name/disable
async fn disable_endpoint(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let flipped = {
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
        let was_enabled = !entry.disabled;
        entry.disabled = true;
        was_enabled
    };
    state.registry.invalidate_catalog_cache().await;
    persist_disabled_state(&state).await;
    if flipped {
        state.registry.tick_tools_changed(&name);
    }
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
    let (flipped, result) = {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        let was_disabled = entry.disabled;
        entry.disabled = false;
        (was_disabled, entry.adapter.initialize().await)
    };
    state.registry.invalidate_catalog_cache().await;
    persist_disabled_state(&state).await;
    if flipped {
        state.registry.tick_tools_changed(&name);
    }
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
    let flipped = {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        entry.disabled_tools.insert(tool_name.clone())
    };
    state.registry.invalidate_catalog_cache().await;
    persist_disabled_state(&state).await;
    if flipped {
        state.registry.tick_tools_changed(&name);
    }
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
    let flipped = {
        let mut entries = state.registry.entries().write().await;
        let Some(entry) = entries.get_mut(&name) else {
            return endpoint_not_found(&name).into_response();
        };
        entry.disabled_tools.remove(&tool_name)
    };
    state.registry.invalidate_catalog_cache().await;
    persist_disabled_state(&state).await;
    if flipped {
        state.registry.tick_tools_changed(&name);
    }
    Json(ActionResponse {
        ok: true,
        message: format!("Tool '{}' enabled on '{}'", tool_name, name),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, FailedAdapter, HealthStatus, McpAdapter, ToolInfo};
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

    /// Mock adapter whose `shutdown` sleeps for a configurable duration and
    /// optionally flips an `AtomicBool` once shutdown is invoked. Used to
    /// verify that `restart_endpoint` returns without awaiting shutdown and
    /// that the background task actually calls `shutdown()` on the old
    /// adapter (rather than dropping it).
    struct SlowShutdownAdapter {
        shutdown_delay: std::time::Duration,
        shutdown_called: Option<Arc<std::sync::atomic::AtomicBool>>,
    }

    impl SlowShutdownAdapter {
        fn new(shutdown_delay: std::time::Duration) -> Self {
            Self {
                shutdown_delay,
                shutdown_called: None,
            }
        }

        fn with_shutdown_flag(
            shutdown_delay: std::time::Duration,
            flag: Arc<std::sync::atomic::AtomicBool>,
        ) -> Self {
            Self {
                shutdown_delay,
                shutdown_called: Some(flag),
            }
        }
    }

    #[async_trait]
    impl McpAdapter for SlowShutdownAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(vec![])
        }
        async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, AdapterError> {
            Ok(Value::Null)
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            if let Some(flag) = &self.shutdown_called {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            tokio::time::sleep(self.shutdown_delay).await;
            Ok(())
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
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
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
            token_manager: None,
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
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
            ..Default::default()
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
        // Field is always serialized (null while detection is in flight).
        assert!(body
            .as_object()
            .unwrap()
            .contains_key("container_runtime_available"));
    }

    #[tokio::test]
    async fn management_status_reports_container_runtime() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);

        // Detection runs on a background thread warmed by management_routes;
        // poll until it resolves to a boolean, then check it matches the
        // process-cached detector result (host-dependent true/false).
        let mut value = None;
        for _ in 0..100 {
            let resp = app
                .clone()
                .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = body_json(resp).await;
            if let Some(b) = body["container_runtime_available"].as_bool() {
                value = Some(b);
                break;
            }
            assert!(body["container_runtime_available"].is_null());
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let available = value.expect("container_runtime_available never resolved to a boolean");
        assert_eq!(
            available,
            crate::container_runtime::detect_runtime().is_some()
        );
    }

    #[tokio::test]
    async fn management_endpoints_list() {
        let tools = vec![
            ToolInfo {
                name: "t1".into(),
                description: None,
                input_schema: serde_json::json!({}),
                annotations: None,
                ..Default::default()
            },
            ToolInfo {
                name: "t2".into(),
                description: None,
                input_schema: serde_json::json!({}),
                annotations: None,
                ..Default::default()
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
    async fn management_endpoints_list_surfaces_ema_auth_binding() {
        let state = test_state(vec![
            ("github-acme", MockAdapter::healthy_with_tools(vec![])),
            ("plain", MockAdapter::healthy_with_tools(vec![])),
        ])
        .await;
        // Replace the seeded config so the listing has a matching EMA endpoint
        // and an ordinary one to compare against.
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints = vec![
                EndpointConfig {
                    name: "github-acme".to_string(),
                    description: None,
                    tool_prefix: None,
                    transport: Transport::Http,
                    command: None,
                    args: None,
                    url: Some("https://api.githubcopilot.com/mcp/".to_string()),
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
                    isolation: None,
                    container_image: None,
                    mounts: None,
                    auth: Some(crate::config::EndpointAuthConfig {
                        auth_type: "ema".to_string(),
                        organization: Some("Acme Corp".to_string()),
                        idp: None,
                        resource: Some("https://api.githubcopilot.com/mcp/".to_string()),
                    }),
                },
                EndpointConfig {
                    name: "plain".to_string(),
                    description: None,
                    tool_prefix: None,
                    transport: Transport::Stdio,
                    command: Some("echo".to_string()),
                    args: None,
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
                    isolation: None,
                    container_image: None,
                    mounts: None,
                    auth: None,
                },
            ];
        }
        let app = management_routes(state);
        let resp = app
            .oneshot(Request::get("/api/endpoints").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Sorted by name: github-acme, plain.
        assert_eq!(arr[0]["name"], "github-acme");
        assert_eq!(arr[0]["auth"]["type"], "ema");
        assert_eq!(arr[0]["auth"]["organization"], "Acme Corp");
        assert_eq!(
            arr[0]["auth"]["resource"],
            "https://api.githubcopilot.com/mcp/"
        );
        // Ordinary endpoint serializes with no `auth` key at all.
        assert_eq!(arr[1]["name"], "plain");
        assert!(
            arr[1].get("auth").is_none(),
            "non-EMA endpoint must not carry an auth summary, got {:?}",
            arr[1]
        );
    }

    #[tokio::test]
    async fn management_endpoint_tools() {
        let tools = vec![ToolInfo {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: None,
            ..Default::default()
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
    async fn management_config_reload_no_config_path() {
        // test_state has config_path: None, so reload should return ok: false
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
        assert_eq!(body["ok"], false);
        assert_eq!(body["message"], "config_path not configured");
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
            ..Default::default()
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
    async fn management_restart_failed_endpoint() {
        // Register a FailedAdapter under name "echo" (which exists in test_config)
        let registry = AdapterRegistry::new();
        registry
            .register(
                "echo".to_string(),
                Box::new(FailedAdapter::new("original init error".to_string())),
                "stdio".to_string(),
                None,
                Some("echo".to_string()),
            )
            .await;
        let state = ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(test_config())),
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

        // Restart should succeed (rebuilds from config)
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should return 200 OK (the rebuild itself succeeds, even if the new adapter
        // ends up as FailedAdapter because "echo" isn't a real MCP server)
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("restarted"));
    }

    #[tokio::test]
    async fn management_restart_endpoint_not_in_config() {
        // Register a MockAdapter under name "notinconfig" (NOT in test_config)
        let state = test_state(vec![(
            "notinconfig",
            MockAdapter::healthy_with_tools(vec![]),
        )])
        .await;
        let app = management_routes(state);

        // Restart should fall back to existing behavior (shutdown + initialize)
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/notinconfig/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // MockAdapter::initialize returns Ok, so this should succeed
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("restarted"));
    }

    #[tokio::test]
    async fn management_restart_endpoint_non_blocking() {
        use std::time::Duration;

        // Register an adapter whose `shutdown` sleeps long enough that a
        // synchronous restart would visibly block the response.
        let registry = AdapterRegistry::new();
        registry
            .register(
                "echo".to_string(),
                Box::new(SlowShutdownAdapter::new(Duration::from_millis(200))),
                "stdio".to_string(),
                None,
                Some("echo".to_string()),
            )
            .await;
        // Point the config entry at a nonexistent command so the background
        // create_adapter rebuild fails at spawn() with ENOENT — instantly and
        // deterministically — instead of spawning a real `echo` subprocess
        // whose MCP handshake stalls through DISCOVER_PROBE_TIMEOUT plus a
        // stderr drain, the documented source of flakiness on loaded CI.
        let mut cfg = test_config();
        cfg.endpoints[0].command = Some("endara-nonexistent-command-for-tests".to_string());
        cfg.endpoints[0].args = None;
        let state = ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(cfg)),
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
        let registry_for_poll = state.registry.clone();
        let app = management_routes(state);

        let start = Instant::now();
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            elapsed < Duration::from_millis(500),
            "restart should return in <500ms, took {:?}",
            elapsed
        );
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["message"].as_str().unwrap().contains("restarted"));

        // Poll the registry until the background task swaps the adapter.
        // The config points "echo" at a nonexistent command, so create_adapter
        // fails at spawn() and produces a FailedAdapter (Unhealthy) with no
        // handshake timing involved. The generous bound only absorbs CI
        // scheduling noise; the non-blocking invariant is the <500ms response
        // assertion above.
        let swapped = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                {
                    let entries = registry_for_poll.entries().read().await;
                    if let Some(entry) = entries.get("echo") {
                        if matches!(entry.adapter.health(), HealthStatus::Unhealthy(_)) {
                            return;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;

        assert!(
            swapped.is_ok(),
            "registry never reflected new adapter within 20s"
        );
    }

    #[tokio::test]
    async fn restart_endpoint_unknown_endpoint_returns_404_and_does_not_spawn() {
        use std::time::Duration;

        // Register a single healthy adapter under "echo"; the request will
        // target an entirely different name that is NOT in the registry.
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(vec![]))]).await;
        let registry_for_check = state.registry.clone();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/missing-endpoint/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 4xx (specifically 404) — endpoint_not_found returns NOT_FOUND.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "endpoint not found");

        // The registry MUST NOT have been mutated:
        // - "echo" is still its original MockAdapter (Healthy, not Starting).
        // - No new "missing-endpoint" key was inserted.
        {
            let entries = registry_for_check.entries().read().await;
            assert_eq!(entries.len(), 1, "registry should still have one entry");
            let echo = entries.get("echo").expect("echo entry preserved");
            assert!(
                matches!(echo.adapter.health(), HealthStatus::Healthy),
                "echo adapter must not have been replaced with StartingAdapter"
            );
            assert!(
                entries.get("missing-endpoint").is_none(),
                "registry must not contain missing-endpoint"
            );
        }

        // Wait briefly to confirm no background spawn mutates the registry
        // after the response (i.e. no `tokio::spawn` was kicked off).
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let entries = registry_for_check.entries().read().await;
            assert_eq!(entries.len(), 1);
            let echo = entries.get("echo").unwrap();
            assert!(matches!(echo.adapter.health(), HealthStatus::Healthy));
        }
    }

    #[tokio::test]
    async fn restart_endpoint_returns_quickly_during_slow_shutdown() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // Old adapter: 1.5s shutdown delay + a flag that flips when shutdown
        // is actually invoked (Item C — verifies the background task ran
        // shutdown rather than dropping the adapter).
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let registry = AdapterRegistry::new();
        registry
            .register(
                "echo".to_string(),
                Box::new(SlowShutdownAdapter::with_shutdown_flag(
                    Duration::from_millis(1500),
                    shutdown_called.clone(),
                )),
                "stdio".to_string(),
                None,
                Some("echo".to_string()),
            )
            .await;
        // Point the config entry at a nonexistent command so the background
        // create_adapter rebuild fails at spawn() with ENOENT — instantly and
        // deterministically — instead of spawning a real `echo` subprocess
        // whose MCP handshake stalls through DISCOVER_PROBE_TIMEOUT plus a
        // stderr drain, the documented source of flakiness on loaded CI.
        let mut cfg = test_config();
        cfg.endpoints[0].command = Some("endara-nonexistent-command-for-tests".to_string());
        cfg.endpoints[0].args = None;
        let state = ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(cfg)),
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
        let registry_for_poll = state.registry.clone();
        let app = management_routes(state);

        // Item B (1): upper bound on response time that proves the restart
        // returns well before the old adapter's 1.5s shutdown completes
        // (i.e. shutdown is backgrounded). Bound is generous enough to
        // tolerate loaded CI without weakening the non-blocking invariant.
        let start = Instant::now();
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            elapsed < Duration::from_millis(1000),
            "restart should return well before the 1500ms shutdown, took {:?}",
            elapsed
        );
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);

        // Item B (3): immediately after the response, the registry MUST
        // contain the StartingAdapter placeholder — i.e. the swap happened
        // synchronously with the request, not asynchronously.
        {
            let entries = registry_for_poll.entries().read().await;
            let entry = entries.get("echo").expect("echo still in registry");
            assert!(
                matches!(entry.adapter.health(), HealthStatus::Starting),
                "registry should hold a StartingAdapter while shutdown runs, got {:?}",
                entry.adapter.health()
            );
        }

        // Item C: poll the AtomicBool until shutdown() is observed running
        // on the old adapter. The flag flips at the start of shutdown(), so
        // this should fire well before the 1.5s sleep finishes.
        let flag_flipped = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if shutdown_called.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            flag_flipped.is_ok(),
            "background task never invoked shutdown() on the old adapter"
        );

        // Item B (4): once the background spawn completes, the registry
        // should hold the new adapter. The config points "echo" at a
        // nonexistent command, so create_adapter fails at spawn() with ENOENT
        // and produces a FailedAdapter (Unhealthy) with no handshake timing
        // involved — only the fixed 1.5s slow shutdown precedes the swap. The
        // generous bound absorbs CI scheduling noise; the non-blocking
        // invariant is asserted above.
        let final_state = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                {
                    let entries = registry_for_poll.entries().read().await;
                    if let Some(entry) = entries.get("echo") {
                        if matches!(entry.adapter.health(), HealthStatus::Unhealthy(_)) {
                            return;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(
            final_state.is_ok(),
            "registry never reflected final swapped adapter within 20s"
        );
    }

    #[tokio::test]
    async fn restart_endpoint_emits_foreground_and_background_ticks() {
        use std::time::Duration;

        // Use a SlowShutdownAdapter so the foreground placeholder swap is
        // observable as a distinct event from the background re-init swap.
        let registry = AdapterRegistry::new();
        registry
            .register(
                "echo".to_string(),
                Box::new(SlowShutdownAdapter::new(Duration::from_millis(150))),
                "stdio".to_string(),
                None,
                Some("echo".to_string()),
            )
            .await;
        // Deliberately use a config with NO endpoints so restart_endpoint's
        // background task cannot find a matching config entry for "echo" and
        // therefore takes the in-place `old.initialize()` re-init branch
        // instead of `watcher::create_adapter` (which would spawn a real
        // `echo` subprocess and run an MCP handshake that only fails after
        // an internal timeout, the source of flakiness on loaded CI).
        // SlowShutdownAdapter::initialize returns Ok(()) instantly, so the
        // background tick fires deterministically after the 150ms shutdown.
        let mut cfg = test_config();
        cfg.endpoints.clear();
        let state = ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(cfg)),
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

        // Subscribe BEFORE issuing the restart so we don't race the
        // foreground tick. The relay-wide channel carries the endpoint name
        // as its payload.
        let mut rx = state.registry.subscribe_tools_changed();
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

        // First tick: foreground swap to StartingAdapter. It should arrive
        // essentially immediately (well within the shutdown delay); bound is
        // loosened to tolerate loaded CI without changing what is asserted.
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("foreground tick did not arrive within 5s")
            .expect("foreground tick channel closed");
        assert_eq!(first, "echo", "foreground tick should carry endpoint name");

        // Second tick: background in-place re-init swap completion. Fires
        // after the 150ms SlowShutdownAdapter shutdown finishes and
        // `old.initialize()` returns Ok(()) instantly.
        let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("background tick did not arrive within 5s")
            .expect("background tick channel closed");
        assert_eq!(second, "echo", "background tick should carry endpoint name");
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
    async fn management_delete_endpoint_cascades_observability() {
        use crate::observability::payloads::PayloadStore;
        use crate::observability::pipeline::Observability;
        use crate::observability::store::{CallRecord, QueryFilter, Store};

        // Two endpoints on disk; we delete one and assert the other's
        // observability data survives.
        let dir =
            std::env::temp_dir().join(format!("relay-test-delete-cascade-{}", std::process::id()));
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

        // Seed the metadata store + payload buffer for both servers, keyed by
        // request_uid.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
        let row = |uid: &str, server: &str| CallRecord {
            request_uid: Some(uid.to_string()),
            server_name: Some(server.to_string()),
            tool: format!("{server}__do"),
            ts_start: 1000,
            ts_end: 1005,
            duration_ms: 5,
            success: true,
            ..Default::default()
        };
        store
            .insert_batch(&[
                row("echo-1", "echo"),
                row("echo-2", "echo"),
                row("keep-1", "keep-me"),
                row("keep-2", "keep-me"),
            ])
            .unwrap();
        for uid in ["echo-1", "echo-2", "keep-1", "keep-2"] {
            payloads.insert(
                uid,
                &serde_json::json!({"q": uid}),
                &serde_json::json!({"ok": true}),
                false,
            );
        }

        let obs = Observability::new(
            &crate::config::ObservabilityConfig::default(),
            Arc::clone(&store),
            Arc::clone(&payloads),
        );
        let registry = AdapterRegistry::new().with_observability(obs);

        let mut state = test_state(vec![]).await;
        state.registry = Arc::new(registry);
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

        // The deleted server's metadata + payloads are gone.
        let echo_rows = store
            .query(
                &QueryFilter {
                    server_name: Some("echo".to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .unwrap();
        assert!(echo_rows.is_empty(), "echo metadata rows should be deleted");
        assert!(payloads.get("echo-1").is_none());
        assert!(payloads.get("echo-2").is_none());

        // The surviving server is untouched.
        let keep_rows = store
            .query(
                &QueryFilter {
                    server_name: Some("keep-me".to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .unwrap();
        assert_eq!(keep_rows.len(), 2, "keep-me metadata rows should survive");
        assert!(payloads.get("keep-1").is_some());
        assert!(payloads.get("keep-2").is_some());

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
            ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
            },
            ToolInfo {
                name: "write".into(),
                description: Some("Write".into()),
                input_schema: serde_json::json!({"type": "object"}),
                annotations: None,
                ..Default::default()
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
            ..Default::default()
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

    // ---- persist_disabled_state raw-table surgery -------------------------
    //
    // Regression tests for the settings-reset bug: persist_disabled_state used
    // to reserialize the whole typed `Config` to disk, silently dropping any
    // sections the struct doesn't model ([desktop], [desktop.overlay], [meta],
    // unknown keys). It now performs a targeted toml::Table edit, so those
    // sections must survive a disable-server / disable-tool persist verbatim.

    const PRESERVE_SECTIONS_TOML: &str = r#"[relay]
machine_name = "test"

[desktop]
update_channel = "beta"

[desktop.overlay]
enabled = true

[meta]
config_version = 3

[[endpoints]]
name = "srv"
transport = "stdio"
command = "echo"
custom_unknown_key = "keep-me"

[[endpoints]]
name = "other"
transport = "stdio"
command = "echo"
"#;

    /// Build a state whose registry and in-memory config carry `srv`, `other`,
    /// and `ghost` (the latter intentionally absent from the on-disk TOML),
    /// wired to `config_file` seeded with [`PRESERVE_SECTIONS_TOML`].
    async fn persist_test_state(config_file: &std::path::Path) -> ManagementState {
        std::fs::write(config_file, PRESERVE_SECTIONS_TOML).unwrap();
        let mut state = test_state(vec![
            ("srv", MockAdapter::healthy_with_tools(vec![])),
            ("other", MockAdapter::healthy_with_tools(vec![])),
            ("ghost", MockAdapter::healthy_with_tools(vec![])),
        ])
        .await;
        state.config_path = Some(config_file.to_path_buf());
        {
            let mut cfg = state.config.write().await;
            let template = cfg.endpoints[0].clone();
            cfg.endpoints = vec![
                EndpointConfig {
                    name: "srv".into(),
                    ..template.clone()
                },
                EndpointConfig {
                    name: "other".into(),
                    ..template.clone()
                },
                EndpointConfig {
                    name: "ghost".into(),
                    ..template
                },
            ];
        }
        state
    }

    #[tokio::test]
    async fn persist_disabled_state_preserves_unknown_sections_on_server_disable() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = persist_test_state(&config_file).await;

        {
            let mut entries = state.registry.entries().write().await;
            entries.get_mut("srv").unwrap().disabled = true;
        }
        persist_disabled_state(&state).await;

        let written: toml::Table = std::fs::read_to_string(&config_file)
            .unwrap()
            .parse()
            .unwrap();
        let original: toml::Table = PRESERVE_SECTIONS_TOML.parse().unwrap();

        // Unknown top-level sections survive verbatim (parse-and-compare).
        assert_eq!(written["desktop"], original["desktop"]);
        assert_eq!(written["meta"], original["meta"]);
        assert_eq!(written["relay"], original["relay"]);

        // The disabled flip is reflected on the right endpoint only, and the
        // registry-only `ghost` endpoint is NOT re-added to the file.
        let endpoints = written["endpoints"].as_array().unwrap();
        assert_eq!(endpoints.len(), 2);
        let srv = endpoints
            .iter()
            .find(|e| e["name"].as_str() == Some("srv"))
            .unwrap();
        assert_eq!(srv["disabled"].as_bool(), Some(true));
        assert_eq!(srv["custom_unknown_key"].as_str(), Some("keep-me"));
        let other = endpoints
            .iter()
            .find(|e| e["name"].as_str() == Some("other"))
            .unwrap();
        assert_eq!(other["disabled"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn persist_disabled_state_preserves_unknown_sections_on_tool_disable() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = persist_test_state(&config_file).await;

        {
            let mut entries = state.registry.entries().write().await;
            entries
                .get_mut("srv")
                .unwrap()
                .disabled_tools
                .insert("read".into());
        }
        persist_disabled_state(&state).await;

        let written: toml::Table = std::fs::read_to_string(&config_file)
            .unwrap()
            .parse()
            .unwrap();
        let original: toml::Table = PRESERVE_SECTIONS_TOML.parse().unwrap();

        // Unknown top-level sections survive verbatim (parse-and-compare).
        assert_eq!(written["desktop"], original["desktop"]);
        assert_eq!(written["meta"], original["meta"]);

        let endpoints = written["endpoints"].as_array().unwrap();
        assert_eq!(endpoints.len(), 2);
        let srv = endpoints
            .iter()
            .find(|e| e["name"].as_str() == Some("srv"))
            .unwrap();
        let tools: Vec<&str> = srv["disabled_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(tools, vec!["read"]);
        assert_eq!(srv["disabled"].as_bool(), Some(false));
        assert_eq!(srv["custom_unknown_key"].as_str(), Some("keep-me"));
    }

    // ---- tools_changed_tx tick coverage (one tick per real flip; zero on no-op)
    //
    // These tests subscribe to the relay-wide `tools_changed_tx` broadcast
    // BEFORE issuing the management call and assert exactly one tick lands on
    // a real state flip and zero ticks on a repeat (no-op) call. The payload
    // is the endpoint name. Settling delays use a short sleep because
    // `tools_changed_tx.send` is synchronous from the broadcast's standpoint
    // but the management handler performs cache invalidation and disk
    // persistence between the state mutation and the tick.

    async fn post_ok(app: axum::Router, path: &str) {
        let resp = app
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "POST {} expected 200", path);
    }

    fn try_recv_tick(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Option<String> {
        match rx.try_recv() {
            Ok(s) => Some(s),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => None,
            Err(e) => panic!("unexpected broadcast recv error: {e:?}"),
        }
    }

    async fn assert_one_tick(rx: &mut tokio::sync::broadcast::Receiver<String>, endpoint: &str) {
        let tick = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("expected one tools_changed_tx tick within 1s")
            .expect("broadcast recv must succeed");
        assert_eq!(tick, endpoint, "tick payload must be the endpoint name");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            try_recv_tick(rx).is_none(),
            "exactly one tick expected; got a second"
        );
    }

    async fn assert_no_tick(rx: &mut tokio::sync::broadcast::Receiver<String>) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            try_recv_tick(rx).is_none(),
            "no-op mutation must not emit a tools_changed_tx tick"
        );
    }

    #[tokio::test]
    async fn disable_endpoint_ticks_once_on_flip_and_not_on_noop() {
        let tools = vec![ToolInfo {
            name: "t1".into(),
            description: None,
            input_schema: serde_json::json!({}),
            annotations: None,
            ..Default::default()
        }];
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(tools))]).await;
        // Hold the registry Arc so the broadcast sender outlives the router.
        let registry = state.registry.clone();
        let mut rx = registry.subscribe_tools_changed();
        let app = management_routes(state);

        // First disable — real flip, expect exactly one tick.
        post_ok(app.clone(), "/api/endpoints/echo/disable").await;
        assert_one_tick(&mut rx, "echo").await;

        // Second disable — already disabled, expect zero ticks.
        post_ok(app, "/api/endpoints/echo/disable").await;
        assert_no_tick(&mut rx).await;
    }

    #[tokio::test]
    async fn enable_endpoint_ticks_once_on_flip_and_not_on_noop() {
        let tools = vec![ToolInfo {
            name: "t1".into(),
            description: None,
            input_schema: serde_json::json!({}),
            annotations: None,
            ..Default::default()
        }];
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state.clone());

        // Pre-condition: disable so the next enable is a real flip. Subscribe
        // AFTER this so the disable tick doesn't pollute the receiver.
        post_ok(app.clone(), "/api/endpoints/echo/disable").await;
        let mut rx = state.registry.subscribe_tools_changed();

        // Enable — real flip, expect exactly one tick.
        post_ok(app.clone(), "/api/endpoints/echo/enable").await;
        assert_one_tick(&mut rx, "echo").await;

        // Enable again — already enabled, expect zero ticks.
        post_ok(app, "/api/endpoints/echo/enable").await;
        assert_no_tick(&mut rx).await;
    }

    #[tokio::test]
    async fn disable_tool_ticks_once_on_flip_and_not_on_noop() {
        let tools = vec![ToolInfo {
            name: "read".into(),
            description: Some("Read".into()),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: None,
            ..Default::default()
        }];
        let state = test_state(vec![("fs", MockAdapter::healthy_with_tools(tools))]).await;
        // Hold the registry Arc so the broadcast sender outlives the router.
        let registry = state.registry.clone();
        let mut rx = registry.subscribe_tools_changed();
        let app = management_routes(state);

        // First disable — newly inserted, expect exactly one tick.
        post_ok(app.clone(), "/api/endpoints/fs/tools/read/disable").await;
        assert_one_tick(&mut rx, "fs").await;

        // Second disable — already in disabled_tools, expect zero ticks.
        post_ok(app, "/api/endpoints/fs/tools/read/disable").await;
        assert_no_tick(&mut rx).await;
    }

    #[tokio::test]
    async fn enable_tool_ticks_once_on_flip_and_not_on_noop() {
        let tools = vec![ToolInfo {
            name: "read".into(),
            description: Some("Read".into()),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: None,
            ..Default::default()
        }];
        let state = test_state(vec![("fs", MockAdapter::healthy_with_tools(tools))]).await;
        let app = management_routes(state.clone());

        // Pre-condition: disable so the next enable is a real flip. Subscribe
        // AFTER this so the disable tick doesn't pollute the receiver.
        post_ok(app.clone(), "/api/endpoints/fs/tools/read/disable").await;
        let mut rx = state.registry.subscribe_tools_changed();

        // Enable — actually removed, expect exactly one tick.
        post_ok(app.clone(), "/api/endpoints/fs/tools/read/enable").await;
        assert_one_tick(&mut rx, "fs").await;

        // Enable again — not in disabled_tools, expect zero ticks.
        post_ok(app, "/api/endpoints/fs/tools/read/enable").await;
        assert_no_tick(&mut rx).await;
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
            heartbeat_interval_secs: 30,
            probe_timeout_secs: 10,
            probe_failure_threshold: 3,
            server_type_override: None,
            allow_insecure_oauth: false,
            ema: None,
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
            token_manager: Some(token_manager),
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
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
    async fn oauth_status_empty_transition_history() {
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
        let history = body["transition_history"].as_array().unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn oauth_status_with_transition_history() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;

        // Trigger some transitions via the public API (must follow legal transitions)
        // NeedsLogin -> AuthRequired is legal
        inner
            .transition_to(OAuthState::AuthRequired, "test: force auth required")
            .await;
        // AuthRequired -> Refreshing is legal
        inner
            .transition_to(OAuthState::Refreshing, "test: retry refresh")
            .await;

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
        let history = body["transition_history"].as_array().unwrap();
        assert_eq!(history.len(), 2);

        // First transition: NeedsLogin -> AuthRequired
        assert_eq!(history[0]["from"], "NeedsLogin");
        assert_eq!(history[0]["to"], "AuthRequired");
        assert_eq!(history[0]["reason"], "test: force auth required");
        assert!(history[0]["ago_ms"].is_number());

        // Second transition: AuthRequired -> Refreshing
        assert_eq!(history[1]["from"], "AuthRequired");
        assert_eq!(history[1]["to"], "Refreshing");
        assert_eq!(history[1]["reason"], "test: retry refresh");
        assert!(history[1]["ago_ms"].is_number());
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

    /// Reset sequencing: disconnect (tokens deleted, state → Disconnected)
    /// happens first, then a fresh start flow with forced consent returns an
    /// authorize URL containing prompt=consent.
    #[tokio::test]
    async fn oauth_reset_disconnects_then_returns_consent_url() {
        // Mock AS for the post-disconnect start flow.
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let base = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                "token_endpoint": format!("{}/token", base),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        // Config half: endpoint "oauth-ep" with oauth_server_url → mock AS.
        // Clear the config client_id: the credentials live ONLY in the DCR
        // record (the manually-supplied-credentials shape), so a reset that
        // dropped the record would come back `dcr_unsupported`.
        let (start_state, _flow_mgr) = test_state_oauth_start("oauth-ep", &base_url, None);
        start_state.config.write().await.endpoints[0].client_id = None;

        // Adapter half: a live OAuth inner with persisted tokens.
        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        // The old grant carries a scope: if the start half ran BEFORE the
        // disconnect, scope accumulation would merge it into the new
        // authorize URL — its absence below proves the ordering.
        let token_set = crate::token_manager::TokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            expires_at: None,
            token_type: "Bearer".to_string(),
            scope: Some("pre-reset-scope".to_string()),
            issued_at: None,
        };
        token_manager.save("oauth-ep", &token_set).await.unwrap();
        // Manually supplied client credentials, stored only in the DCR
        // record: reset must preserve them (only the grant is discarded).
        let manual_creds = DcrCredentials {
            client_id: "manual-client".to_string(),
            client_secret: Some("manual-secret".to_string()),
            registered_via_dcr: false,
            ..Default::default()
        };
        token_manager
            .save_dcr("oauth-ep", &manual_creds)
            .await
            .unwrap();
        let adapter = OAuthAdapter::new(make_oauth_config("oauth-ep"), token_manager.clone());
        let shared_inner = adapter.shared_inner();
        *shared_inner.state.write().await = OAuthState::Authenticated;
        let oauth_inners: OAuthAdapterInners =
            Arc::new(RwLock::new(std::collections::HashMap::new()));
        oauth_inners
            .write()
            .await
            .insert("oauth-ep".to_string(), shared_inner.clone());
        let registry = AdapterRegistry::new();
        registry
            .register(
                "oauth-ep".to_string(),
                Box::new(adapter),
                "oauth".to_string(),
                None,
                None,
            )
            .await;

        let state = ManagementState {
            registry: Arc::new(registry),
            oauth_adapter_inners: Some(oauth_inners),
            token_manager: Some(token_manager.clone()),
            ..start_state
        };

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/oauth-ep/oauth/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.contains("&prompt=consent"),
            "reset must force consent, got: {}",
            authorize_url
        );

        // Disconnect ran before the start flow: local tokens are gone and
        // the adapter is Disconnected.
        assert_eq!(*shared_inner.state.read().await, OAuthState::Disconnected);
        assert!(token_manager.load("oauth-ep").await.unwrap().is_none());

        // Ordering proof: had the start half run before the disconnect,
        // scope accumulation would have merged the old grant's scope into
        // the new authorize URL.
        assert!(
            !authorize_url.contains("pre-reset-scope"),
            "old grant's scope leaked into the reset URL — start ran before disconnect: {}",
            authorize_url
        );

        // ...but the client registration survives the reset, and the new
        // authorize URL was built with the preserved client_id.
        let creds = token_manager
            .load_dcr("oauth-ep")
            .await
            .unwrap()
            .expect("client registration must be preserved across reset");
        assert_eq!(creds.client_id, "manual-client");
        assert_eq!(creds.client_secret.as_deref(), Some("manual-secret"));
        assert!(
            authorize_url.contains("client_id=manual-client"),
            "start flow must reuse the preserved client registration, got: {}",
            authorize_url
        );
    }

    #[tokio::test]
    async fn oauth_reset_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _inner) = test_state_with_oauth("oauth-ep", tmp.path()).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/nonexistent/oauth/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_reset_not_oauth_endpoint() {
        // Non-OAuth endpoint with no adapter inners
        let state = test_state(vec![("echo", MockAdapter::healthy_with_tools(vec![]))]).await;
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/endpoints/echo/oauth/reset")
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

    // -----------------------------------------------------------------------
    // OAuth setup (preflight) route tests
    // -----------------------------------------------------------------------

    /// Helper: create a ManagementState with setup_manager and flow_manager.
    async fn test_state_with_setup() -> ManagementState {
        let registry = AdapterRegistry::new();
        ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(test_config())),
            start_time: Instant::now(),
            config_path: None,
            oauth_flow_manager: Some(Arc::new(OAuthFlowManager::new())),
            relay_port: 9400,
            oauth_adapter_inners: None,
            token_manager: None,
            setup_manager: Some(Arc::new(OAuthSetupManager::new())),
            profile_registry: None,
            event_bus: None,
        }
    }

    #[tokio::test]
    async fn oauth_setup_status_invalid_session_returns_not_found() {
        let state = test_state_with_setup().await;
        let fake_id = uuid::Uuid::new_v4();
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get(format!("/api/oauth/setup/{}/status", fake_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_setup_status_bad_uuid_returns_bad_request() {
        let state = test_state_with_setup().await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/oauth/setup/not-a-uuid/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oauth_setup_cancel_nonexistent_returns_not_found() {
        let state = test_state_with_setup().await;
        let fake_id = uuid::Uuid::new_v4();
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::delete(format!("/api/oauth/setup/{}", fake_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_setup_cancel_existing_session() {
        let state = test_state_with_setup().await;
        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::delete(format!("/api/oauth/setup/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "cancelled");
    }

    #[tokio::test]
    async fn oauth_setup_cancel_then_cancel_again_returns_not_found() {
        let state = test_state_with_setup().await;
        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        // First cancel
        setup_mgr.remove_session(&session_id).await;

        // Second cancel via route
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::delete(format!("/api/oauth/setup/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_setup_commit_nonexistent_returns_not_found() {
        let state = test_state_with_setup().await;
        let fake_id = uuid::Uuid::new_v4();
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", fake_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_setup_commit_not_authorized_returns_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path);

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        // Session is in AwaitingCredentials status (not Authorized)
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "session_not_authorized");
    }

    #[tokio::test]
    async fn oauth_setup_commit_after_cancel_returns_not_found() {
        let state = test_state_with_setup().await;
        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        // Cancel the session
        setup_mgr.remove_session(&session_id).await;

        // Attempt to commit
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_setup_status_valid_session() {
        let state = test_state_with_setup().await;
        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session(
                "my-ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get(format!("/api/oauth/setup/{}/status", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["session_id"], session_id.to_string());
        assert_eq!(body["name"], "my-ep");
        assert_eq!(body["url"], "https://mcp.example.com");
        assert_eq!(body["status"], "awaiting_credentials");
    }

    #[tokio::test]
    async fn oauth_setup_credentials_nonexistent_session_returns_not_found() {
        let state = test_state_with_setup().await;
        let fake_id = uuid::Uuid::new_v4();
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/credentials", fake_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_id": "my-client",
                            "client_secret": "my-secret"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oauth_setup_credentials_empty_client_id_returns_bad_request() {
        let state = test_state_with_setup().await;
        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();
        // Set auth/token endpoints so credential submission can proceed
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/credentials", session_id))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_id": "   ",
                            "client_secret": null
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn oauth_setup_commit_happy_path_writes_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path.clone());
        state.token_manager = Some(Arc::new(TokenManager::new(token_dir)));

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session(
                "new-ep".into(),
                "https://mcp.example.com".into(),
                Some("read write".into()),
                Some("newep".into()),
                None,
            )
            .await
            .unwrap();

        // Set up the session as Authorized with endpoints and tokens
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
                s.oauth_server_url = Some("https://auth.example.com".into());
                s.client_id = Some("client-123".into());
                s.client_secret = Some("secret-456".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
                s.tokens = Some(crate::token_manager::TokenSet {
                    access_token: "access-tok".into(),
                    refresh_token: Some("refresh-tok".into()),
                    expires_at: Some(9999999999),
                    token_type: "Bearer".into(),
                    scope: Some("read write".into()),
                    issued_at: None,
                });
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"], "committed");
        assert_eq!(body["name"], "new-ep");

        // Verify config was written
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(contents.contains("new-ep"));
        assert!(contents.contains("https://mcp.example.com"));
        assert!(contents.contains("oauth"));
        assert!(contents.contains("client-123"));
        // The secret must never be stamped into config.toml.
        assert!(
            !contents.contains("client_secret"),
            "client_secret must not be written to config.toml; got:\n{}",
            contents
        );
        assert!(!contents.contains("secret-456"));
    }

    /// A successful commit consumes the session (releasing its name
    /// reservation): a second commit of the same session returns 404, and a
    /// new setup session can reuse the name.
    #[tokio::test]
    async fn oauth_setup_commit_consumes_session_and_releases_name() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path);

        let setup_mgr = state.setup_manager.as_ref().unwrap().clone();
        let session_id = setup_mgr
            .create_session(
                "ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.client_id = Some("cid".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Session consumed: re-commit is a 404.
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Name reservation released (config-level duplicate detection is a
        // separate concern checked by the setup-start handler).
        assert!(setup_mgr
            .create_session(
                "ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None
            )
            .await
            .is_some());
    }

    /// A failed commit (session not authorized) keeps the session — and its
    /// name reservation — in place for a retry or cancel.
    #[tokio::test]
    async fn oauth_setup_commit_failure_keeps_session_and_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path);

        let setup_mgr = state.setup_manager.as_ref().unwrap().clone();
        let session_id = setup_mgr
            .create_session(
                "ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        // Not authorized → commit must fail with 409.

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // The session survives the failed commit…
        assert!(setup_mgr.get_session(&session_id, |_| ()).await.is_some());
        // …and keeps holding the name reservation.
        assert!(setup_mgr
            .create_session(
                "ep".into(),
                "https://other.example.com".into(),
                None,
                None,
                None
            )
            .await
            .is_none());
    }

    /// Regression: an authorized session carrying a `client_secret` commits
    /// with `client_id` (but no `client_secret` key) in the written TOML
    /// entry, and the secret lands in the DCR store via the defensive save
    /// (no record was written during setup).
    #[tokio::test]
    async fn oauth_setup_commit_persists_secret_to_dcr_store_not_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();
        let token_manager = Arc::new(TokenManager::new(token_dir));

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path.clone());
        state.token_manager = Some(token_manager.clone());

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session(
                "secret-ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
                s.issuer = Some("https://auth.example.com".into());
                s.client_id = Some("client-123".into());
                s.client_secret = Some("super-secret".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
                s.tokens = Some(crate::token_manager::TokenSet {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    token_type: "Bearer".into(),
                    scope: None,
                    issued_at: None,
                });
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // TOML entry: `client_id` present, `client_secret` key absent.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let parsed: toml::Table = contents.parse().unwrap();
        let endpoints = parsed
            .get("endpoints")
            .and_then(|v| v.as_array())
            .expect("endpoints array missing from written config");
        let ep = endpoints
            .iter()
            .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("secret-ep"))
            .expect("committed endpoint missing from config");
        assert_eq!(
            ep.get("client_id").and_then(|v| v.as_str()),
            Some("client-123")
        );
        assert!(
            ep.get("client_secret").is_none(),
            "client_secret must not be written to config.toml; got:\n{}",
            contents
        );
        assert!(!contents.contains("super-secret"));

        // The defensive save persisted the credentials to the DCR store.
        let creds = token_manager
            .load_dcr("secret-ep")
            .await
            .unwrap()
            .expect("commit must defensively persist credentials to the DCR store");
        assert_eq!(creds.client_id, "client-123");
        assert_eq!(creds.client_secret.as_deref(), Some("super-secret"));
        // Non-DCR-provenanced credentials are stored issuer-unbound, matching
        // the manual /credentials path convention.
        assert_eq!(creds.issuer, None);
        assert!(!creds.registered_via_dcr);
    }

    /// Helper: build a commit-ready authorized session and return the app +
    /// paths + token manager. The session's fields are customized by `f`.
    async fn commit_fixture<F>(
        name: &str,
        f: F,
    ) -> (
        axum::Router,
        uuid::Uuid,
        std::path::PathBuf,
        Arc<TokenManager>,
        tempfile::TempDir,
    )
    where
        F: FnOnce(&mut crate::oauth::OAuthSetupSession),
    {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();
        let token_manager = Arc::new(TokenManager::new(token_dir));

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path.clone());
        state.token_manager = Some(token_manager.clone());

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session(
                name.into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
                s.tokens = Some(crate::token_manager::TokenSet {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    token_type: "Bearer".into(),
                    scope: None,
                    issued_at: None,
                });
                f(s);
            })
            .await;

        (
            management_routes(state),
            session_id,
            config_path,
            token_manager,
            tmp,
        )
    }

    /// The defensive save must persist the session's tracked registration
    /// provenance: credentials minted via DCR during setup produce a
    /// `registered_via_dcr: true` record (keeping the RFC 7591 self-heal
    /// paths reachable), not a hardcoded `false`.
    #[tokio::test]
    async fn oauth_setup_commit_defensive_save_preserves_dcr_provenance() {
        let (app, session_id, _config_path, token_manager, _tmp) = commit_fixture("dcr-ep", |s| {
            s.issuer = Some("https://auth.example.com".into());
            s.client_id = Some("dcr-minted".into());
            s.client_secret = Some("dcr-secret".into());
            s.registered_via_dcr = true;
        })
        .await;

        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let creds = token_manager
            .load_dcr("dcr-ep")
            .await
            .unwrap()
            .expect("defensive save must write a DCR record");
        assert_eq!(creds.client_id, "dcr-minted");
        assert_eq!(creds.client_secret.as_deref(), Some("dcr-secret"));
        assert_eq!(creds.issuer.as_deref(), Some("https://auth.example.com"));
        assert!(
            creds.registered_via_dcr,
            "DCR provenance from the session must be persisted"
        );
    }

    /// Save-if-absent: a pre-existing record carrying the session's
    /// credentials is kept verbatim (registered_at / issuer / provenance
    /// untouched) — commit never clobbers it with a fresh defensive record.
    #[tokio::test]
    async fn oauth_setup_commit_keeps_matching_existing_dcr_record_intact() {
        let (app, session_id, _config_path, token_manager, _tmp) = commit_fixture("keep-ep", |s| {
            s.client_id = Some("client-123".into());
            s.client_secret = Some("secret-456".into());
        })
        .await;

        let original = DcrCredentials {
            client_id: "client-123".to_string(),
            client_secret: Some("secret-456".to_string()),
            client_secret_expires_at: 0,
            registered_at: 42,
            issuer: Some("https://original.example.com".to_string()),
            registered_via_dcr: true,
            ..Default::default()
        };
        token_manager.save_dcr("keep-ep", &original).await.unwrap();

        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let loaded = token_manager.load_dcr("keep-ep").await.unwrap().unwrap();
        assert_eq!(
            loaded, original,
            "existing DCR record must be kept verbatim"
        );
    }

    /// A pre-existing record that does NOT match the session's credentials
    /// (stale file after endpoint deletion, or a same-name session) must
    /// reject the commit: otherwise the TOML would carry the session's
    /// `client_id` while credential resolution prefers the store's different
    /// pair. Nothing may be written to config.toml, and the stored record
    /// stays untouched.
    #[tokio::test]
    async fn oauth_setup_commit_rejects_mismatched_dcr_record() {
        let (app, session_id, config_path, token_manager, _tmp) = commit_fixture("mm-ep", |s| {
            s.client_id = Some("session-client".into());
            s.client_secret = Some("session-secret".into());
        })
        .await;

        let stale = DcrCredentials {
            client_id: "stale-client".to_string(),
            client_secret: Some("stale-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 7,
            issuer: Some("https://other.example.com".to_string()),
            registered_via_dcr: true,
            ..Default::default()
        };
        token_manager.save_dcr("mm-ep", &stale).await.unwrap();

        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "dcr_record_mismatch");

        // config.toml untouched — no endpoint entry was committed.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !contents.contains("mm-ep"),
            "commit must not write config.toml on mismatch; got:\n{}",
            contents
        );

        // Stored record untouched.
        let loaded = token_manager.load_dcr("mm-ep").await.unwrap().unwrap();
        assert_eq!(loaded, stale);
    }

    /// A store read/write failure during the defensive save (e.g. a corrupt
    /// `{name}.dcr.json`) must fail the commit: committing anyway would write
    /// a secretless TOML entry whose credentials can't be resolved after a
    /// restart. Nothing may be written to config.toml.
    #[tokio::test]
    async fn oauth_setup_commit_fails_when_dcr_store_unreadable() {
        let (app, session_id, config_path, _token_manager, tmp) = commit_fixture("bad-ep", |s| {
            s.client_id = Some("client-x".into());
            s.client_secret = Some("secret-x".into());
        })
        .await;

        // Corrupt record: load_dcr will fail with a serde error.
        std::fs::write(tmp.path().join("tokens/bad-ep.dcr.json"), "{not json").unwrap();

        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "dcr_persistence_failed");

        // config.toml untouched — no endpoint entry was committed.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !contents.contains("bad-ep"),
            "commit must not write config.toml on store failure; got:\n{}",
            contents
        );
    }

    /// Round-trip: a commit driven by a session created with
    /// `server_type_override = Some(...)` must write the field to config.toml.
    #[tokio::test]
    async fn oauth_setup_commit_persists_server_type_override() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path.clone());
        state.token_manager = Some(Arc::new(TokenManager::new(token_dir)));

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session(
                "drive-ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                Some("google-drive".into()),
            )
            .await
            .unwrap();

        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
                s.client_id = Some("cid".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
                s.tokens = Some(crate::token_manager::TokenSet {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    token_type: "Bearer".into(),
                    scope: None,
                    issued_at: None,
                });
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Re-parse the written config and assert the field made it through.
        // Parse as a raw toml::Table because the test fixture omits the
        // mandatory `[relay]` section that `Config` deserialization requires.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let parsed: toml::Table = contents.parse().unwrap();
        let endpoints = parsed
            .get("endpoints")
            .and_then(|v| v.as_array())
            .expect("endpoints array missing from written config");
        let ep = endpoints
            .iter()
            .find(|v| v.get("name").and_then(|n| n.as_str()) == Some("drive-ep"))
            .expect("committed endpoint missing from config");
        assert_eq!(
            ep.get("server_type_override").and_then(|v| v.as_str()),
            Some("google-drive"),
            "server_type_override missing from written endpoint entry"
        );
    }

    /// Sanity check: when no override is supplied, the commit must not emit a
    /// `server_type_override` key (avoids polluting config.toml with `None`).
    #[tokio::test]
    async fn oauth_setup_commit_omits_server_type_override_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path.clone());
        state.token_manager = Some(Arc::new(TokenManager::new(token_dir)));

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session(
                "plain-ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
                s.client_id = Some("cid".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
                s.tokens = Some(crate::token_manager::TokenSet {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    token_type: "Bearer".into(),
                    scope: None,
                    issued_at: None,
                });
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !contents.contains("server_type_override"),
            "expected no server_type_override key when override is None, got:\n{}",
            contents
        );
    }

    /// The defensive save must carry the expiry resolved during setup
    /// (tracked on the session), not a hardcoded "never expires": an
    /// expiring DCR secret recovered at commit time keeps its lifetime.
    #[tokio::test]
    async fn oauth_setup_commit_defensive_save_preserves_secret_expiry() {
        let (app, session_id, _config_path, token_manager, _tmp) = commit_fixture("exp-ep", |s| {
            s.client_id = Some("dcr-minted".into());
            s.client_secret = Some("dcr-secret".into());
            s.client_secret_expires_at = 1_999_999_999;
            s.registered_via_dcr = true;
        })
        .await;

        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let creds = token_manager
            .load_dcr("exp-ep")
            .await
            .unwrap()
            .expect("defensive save must write a DCR record");
        assert_eq!(
            creds.client_secret_expires_at, 1_999_999_999,
            "the session's resolved secret expiry must be persisted"
        );
    }

    /// While a commit has claimed the session, a duplicate commit gets
    /// `409 commit_in_progress` and a cancel gets `409` too — neither can
    /// consume the in-flight session.
    #[tokio::test]
    async fn oauth_setup_commit_claim_blocks_duplicate_commit_and_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path);

        let setup_mgr = state.setup_manager.as_ref().unwrap().clone();
        let session_id = setup_mgr
            .create_session(
                "busy-ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.status = crate::oauth::SetupSessionStatus::Authorized;
            })
            .await;

        // Simulate an in-flight commit holding the claim.
        assert!(matches!(
            setup_mgr.claim_for_commit(&session_id).await,
            crate::oauth::CommitClaim::Claimed(_)
        ));

        let app = management_routes(state);
        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "commit_in_progress");

        let resp = app
            .oneshot(
                Request::delete(format!("/api/oauth/setup/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // The session is still there for the in-flight commit.
        assert!(setup_mgr.get_session(&session_id, |_| ()).await.is_some());
    }

    /// The committed endpoint must be visible through `state.config` the
    /// moment the commit response lands — published synchronously, not via
    /// the watcher's debounced reload — so a same-name setup started right
    /// after the commit is rejected by the duplicate-name check instead of
    /// slipping through and clobbering the endpoint's DCR record.
    #[tokio::test]
    async fn oauth_setup_commit_publishes_config_before_releasing_name() {
        let (app, session_id, _config_path, _token_manager, _tmp) =
            commit_fixture("sync-ep", |s| {
                s.client_id = Some("client-123".into());
                s.client_secret = Some("secret-456".into());
            })
            .await;

        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // A new same-name setup right after the commit response must be
        // rejected by the duplicate-name check against `state.config` —
        // no watcher involved.
        let resp = app
            .oneshot(
                Request::post("/api/oauth/setup")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "sync-ep",
                            "url": "https://other.example.com"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "endpoint_exists");
    }

    /// A failed commit releases the claim: the session returns to
    /// `Authorized` so it can be retried or cancelled.
    #[tokio::test]
    async fn oauth_setup_commit_failure_releases_claim() {
        // config_path is None → the claimed commit fails immediately.
        let state = test_state_with_setup().await;
        let setup_mgr = state.setup_manager.as_ref().unwrap().clone();
        let session_id = setup_mgr
            .create_session(
                "rel-ep".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.status = crate::oauth::SetupSessionStatus::Authorized;
            })
            .await;

        let app = management_routes(state);
        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Claim released: session is Authorized again and cancellable.
        let status = setup_mgr
            .get_session(&session_id, |s| s.status.clone())
            .await
            .unwrap();
        assert_eq!(status, crate::oauth::SetupSessionStatus::Authorized);

        let resp = app
            .oneshot(
                Request::delete(format!("/api/oauth/setup/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// HTTP-layer round-trip: the request body's `server_type_override` field
    /// must deserialize and be threaded into the resulting setup session.
    #[tokio::test]
    async fn oauth_setup_request_threads_server_type_override_into_session() {
        // Deserialize via the public JSON contract and verify the field lands
        // on the request struct that the handler uses.
        let body: OAuthSetupRequest = serde_json::from_value(serde_json::json!({
            "name": "drive-ep",
            "url": "https://mcp.example.com",
            "server_type_override": "google-drive"
        }))
        .unwrap();
        assert_eq!(body.server_type_override.as_deref(), Some("google-drive"));

        // And when omitted, it defaults to None.
        let body_no_override: OAuthSetupRequest = serde_json::from_value(serde_json::json!({
            "name": "x",
            "url": "https://x.com"
        }))
        .unwrap();
        assert!(body_no_override.server_type_override.is_none());
    }

    #[tokio::test]
    async fn oauth_setup_double_commit_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();

        let mut state = test_state_with_setup().await;
        state.config_path = Some(config_path.clone());
        state.token_manager = Some(Arc::new(TokenManager::new(token_dir)));

        let setup_mgr = state.setup_manager.as_ref().unwrap();
        let session_id = setup_mgr
            .create_session("ep".into(), "https://x.com".into(), None, None, None)
            .await
            .unwrap();

        // Mark as authorized
        setup_mgr
            .get_session_mut(&session_id, |s| {
                s.authorization_endpoint = Some("https://auth.example.com/authorize".into());
                s.token_endpoint = Some("https://auth.example.com/token".into());
                s.client_id = Some("cid".into());
                s.status = crate::oauth::SetupSessionStatus::Authorized;
                s.tokens = Some(crate::token_manager::TokenSet {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    token_type: "Bearer".into(),
                    scope: None,
                    issued_at: None,
                });
            })
            .await;

        // First commit succeeds (consumes session)
        let app = management_routes(state.clone());
        let resp = app
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Second commit should fail — session already consumed
        let app2 = management_routes(state);
        let resp2 = app2
            .oneshot(
                Request::post(format!("/api/oauth/setup/{}/commit", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::NOT_FOUND);
    }

    // --- Token expiry computation tests ---

    #[test]
    fn expires_in_seconds_computed_correctly() {
        // Simulates the computation from oauth_status_detailed handler
        let now_secs: u64 = 1_700_000_000;
        let expires_at: Option<u64> = Some(now_secs + 3600);
        let expires_in_seconds = expires_at.map(|exp| exp as i64 - now_secs as i64);
        assert_eq!(expires_in_seconds, Some(3600));
    }

    #[test]
    fn expires_in_seconds_negative_when_expired() {
        let now_secs: u64 = 1_700_000_000;
        let expires_at: Option<u64> = Some(now_secs - 100);
        let expires_in_seconds = expires_at.map(|exp| exp as i64 - now_secs as i64);
        assert_eq!(expires_in_seconds, Some(-100));
    }

    #[test]
    fn next_refresh_at_75_percent_rule() {
        // Simulates the computation from oauth_status_detailed handler
        let issued: u64 = 1_700_000_000;
        let expires: u64 = issued + 3600; // 1-hour token
        let lifetime = expires - issued;
        let seventy_five_pct = issued + (lifetime * 3 / 4);
        let five_min_before = expires.saturating_sub(300);
        let next_refresh = std::cmp::min(seventy_five_pct, five_min_before);
        // 75% of 3600 = 2700; 5 min before = 3300. min = 2700.
        assert_eq!(next_refresh, issued + 2700);
    }

    #[test]
    fn next_refresh_at_short_token() {
        // For a 10-minute token: 75% = 450s, 5-min-before = 300s. min = 300.
        let issued: u64 = 1_700_000_000;
        let expires: u64 = issued + 600;
        let lifetime = expires - issued;
        let seventy_five_pct = issued + (lifetime * 3 / 4);
        let five_min_before = expires.saturating_sub(300);
        let next_refresh = std::cmp::min(seventy_five_pct, five_min_before);
        assert_eq!(next_refresh, issued + 300);
    }

    #[test]
    fn next_refresh_at_none_when_no_issued() {
        // If issued_at is None, next_refresh_at should be None
        let issued_at: Option<u64> = None;
        let expires_at: Option<u64> = Some(1_700_003_600);
        let next_refresh = match (issued_at, expires_at) {
            (Some(issued), Some(expires)) if expires > issued => {
                let lifetime = expires - issued;
                let seventy_five_pct = issued + (lifetime * 3 / 4);
                let five_min_before = expires.saturating_sub(300);
                Some(std::cmp::min(seventy_five_pct, five_min_before))
            }
            _ => None,
        };
        assert_eq!(next_refresh, None);
    }

    #[test]
    fn token_with_expires_in_produces_correct_expires_at() {
        // Simulates the token construction in server.rs
        let now_secs: u64 = 1_700_000_000;
        let expires_in: u64 = 3600;
        let expires_at = now_secs + expires_in;
        assert_eq!(expires_at, 1_700_003_600);
    }

    // -----------------------------------------------------------------------
    // /api/endpoints/{name}/credentials route tests (Wave 3a)
    // -----------------------------------------------------------------------

    /// Build a `ManagementState` with a `TokenManager` rooted in `tmp_dir` and
    /// a single endpoint named `name` in the config (and no OAuth adapter
    /// registered — the credentials routes do not require one).
    async fn test_state_with_token_manager(
        name: &str,
        tmp_dir: &std::path::Path,
        config_secret: Option<&str>,
        config_oauth_server_url: Option<&str>,
    ) -> (ManagementState, Arc<TokenManager>) {
        let token_manager = Arc::new(TokenManager::new(tmp_dir.to_path_buf()));
        let cfg = Config {
            relay: RelayConfig {
                machine_name: "test-machine".to_string(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: None,
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints: vec![EndpointConfig {
                name: name.to_string(),
                description: None,
                tool_prefix: None,
                transport: Transport::Oauth,
                command: None,
                args: None,
                url: Some("https://mcp.example.com".to_string()),
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: config_oauth_server_url.map(|s| s.to_string()),
                client_id: Some("legacy-client-id".to_string()),
                client_secret: config_secret.map(|s| s.to_string()),
                scopes: None,
                token_endpoint: None,
                server_type_override: None,
                isolation: None,
                container_image: None,
                mounts: None,
                auth: None,
            }],
            profiles: None,
            organizations: Vec::new(),
        };
        let state = ManagementState {
            registry: Arc::new(AdapterRegistry::new()),
            config: Arc::new(RwLock::new(cfg)),
            start_time: Instant::now(),
            config_path: None,
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: None,
            token_manager: Some(token_manager.clone()),
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
        };
        (state, token_manager)
    }

    #[tokio::test]
    async fn credentials_endpoint_persists_via_token_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_id": "new-client",
                            "client_secret": "new-secret",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["client_secret_set"], true);

        let loaded = tm.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "new-client");
        assert_eq!(loaded.client_secret.as_deref(), Some("new-secret"));
    }

    #[tokio::test]
    async fn credentials_endpoint_rejects_missing_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "client_secret": "x" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn credentials_endpoint_rejects_missing_resource_client_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "resource_client_secret": "x" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn credentials_endpoint_unknown_endpoint_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/nope/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "client_id": "x" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_credentials_prefers_dcr_over_config() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, tm) = test_state_with_token_manager(
            "ep1",
            tmp.path(),
            Some("legacy-secret"),
            Some("https://auth.example.com"),
        )
        .await;
        // Seed DCR with a different client_id and a secret.
        tm.save_dcr(
            "ep1",
            &DcrCredentials {
                client_id: "dcr-client".to_string(),
                client_secret: Some("dcr-secret".to_string()),
                client_secret_expires_at: 0,
                registered_at: 1_700_000_000,
                issuer: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/ep1/credentials")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["client_id"], "dcr-client");
        assert_eq!(body["client_secret_set"], true);
        assert_eq!(body["source"], "dcr");
        assert_eq!(body["oauth_server_url"], "https://auth.example.com");
        // Secret value must NOT be returned.
        assert!(body.get("client_secret").is_none());
    }

    #[tokio::test]
    async fn get_credentials_falls_back_to_config_when_no_dcr() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _tm) =
            test_state_with_token_manager("ep1", tmp.path(), Some("legacy-secret"), None).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/ep1/credentials")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["client_id"], "legacy-client-id");
        assert_eq!(body["client_secret_set"], true);
        assert_eq!(body["source"], "config");
        assert!(body.get("client_secret").is_none());
    }

    /// R3: the optional EMA **resource** credential pair is captured + persisted
    /// **per-endpoint** in `{name}.dcr.json` via POST
    /// /api/endpoints/{name}/credentials, with absent=keep / empty=clear /
    /// non-empty=set semantics. A resource-only update (no requesting client_id)
    /// is allowed, and each field merges independently.
    #[tokio::test]
    async fn endpoint_credentials_resource_pair_persist_and_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;
        let app = management_routes(state);

        // 1. SET — resource-only update (no requesting client_id) is allowed.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "resource_client_id": "res-client",
                            "resource_client_secret": "res-secret",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm
            .load_dcr("ep1")
            .await
            .unwrap()
            .expect("endpoint DCR should exist after set");
        assert_eq!(creds.resource_client_id.as_deref(), Some("res-client"));
        assert_eq!(creds.resource_client_secret.as_deref(), Some("res-secret"));
        // Resource-only: no requesting client_id/secret was supplied.
        assert!(creds.client_id.is_empty());
        assert!(creds.client_secret.is_none());

        // 2. Update ONLY the resource secret — resource id preserved.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"resource_client_secret": "res-secret-v2"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(
            creds.resource_client_id.as_deref(),
            Some("res-client"),
            "resource_client_id must survive a resource-secret update"
        );
        assert_eq!(
            creds.resource_client_secret.as_deref(),
            Some("res-secret-v2")
        );

        // 3. Add requesting client_id + secret — resource pair preserved.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_id": "req-client",
                            "client_secret": "req-secret",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(creds.client_id, "req-client");
        assert_eq!(creds.client_secret.as_deref(), Some("req-secret"));
        assert_eq!(
            creds.resource_client_id.as_deref(),
            Some("res-client"),
            "resource pair must survive a requesting-cred update"
        );
        assert_eq!(
            creds.resource_client_secret.as_deref(),
            Some("res-secret-v2")
        );

        // 4. Clear ONLY the resource secret — everything else kept.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"resource_client_secret": ""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(creds.client_id, "req-client");
        assert_eq!(creds.client_secret.as_deref(), Some("req-secret"));
        assert_eq!(creds.resource_client_id.as_deref(), Some("res-client"));
        assert!(
            creds.resource_client_secret.is_none(),
            "resource_client_secret must be cleared by an empty string"
        );

        // 5. GET surfaces the resource id + secret-set flag (never the secret).
        let _ = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"resource_client_secret": "res-secret-v3"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/endpoints/ep1/credentials")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["resource_client_id"], "res-client");
        assert_eq!(body["resource_client_secret_set"], true);
        assert!(body.get("resource_client_secret").is_none());
    }

    /// R3: clearing the last remaining credential removes the per-endpoint DCR
    /// record entirely (back to the no-credential state).
    #[tokio::test]
    async fn endpoint_credentials_clearing_last_removes_dcr() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;
        let app = management_routes(state);
        // Seed a resource-only record.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"resource_client_id": "res-client"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(tm.load_dcr("ep1").await.unwrap().is_some());
        // Clear it — the record should be removed.
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"resource_client_id": ""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tm.load_dcr("ep1").await.unwrap().is_none(),
            "DCR file should be removed when the last credential is cleared"
        );
    }

    /// Regression for PR #130 Finding 4: replacing a DCR-provenanced
    /// requesting pair with a manual pair must clear the stored `issuer`
    /// binding, not just flip `registered_via_dcr = false`. `oauth_start`
    /// rejects a persisted DCR record whose `issuer` differs from the
    /// currently discovered one BEFORE it checks the provenance flag, so
    /// leaving the DCR-era issuer on the record would let a later issuer
    /// change silently discard the manual credentials and overwrite them
    /// with a fresh DCR registration — breaking the "manual credentials
    /// survive" promise this endpoint makes.
    #[tokio::test]
    async fn endpoint_credentials_manual_replace_clears_issuer_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;

        // Seed a DCR-provenanced record with an `issuer` binding.
        let stored = DcrCredentials {
            client_id: "old-dcr-client".to_string(),
            client_secret: Some("old-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 42,
            issuer: Some("https://as.example.com".to_string()),
            resource_client_id: Some("res-client".to_string()),
            resource_client_secret: Some("res-secret".to_string()),
            registered_via_dcr: true,
        };
        tm.save_dcr("ep1", &stored).await.unwrap();

        let app = management_routes(state);
        // Manually replace the requesting pair only.
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_id": "manual-client",
                            "client_secret": "manual-secret",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let loaded = tm.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "manual-client");
        assert_eq!(loaded.client_secret.as_deref(), Some("manual-secret"));
        assert!(
            !loaded.registered_via_dcr,
            "manual replace must flip the provenance flag off"
        );
        assert!(
            loaded.issuer.is_none(),
            "manual replace must clear the DCR-era issuer binding so a later \
             issuer change does not silently discard the manual credentials"
        );
        // The MAS resource pair is untouched by a requesting-pair replace.
        assert_eq!(loaded.resource_client_id.as_deref(), Some("res-client"));
        assert_eq!(loaded.resource_client_secret.as_deref(), Some("res-secret"));
    }

    /// A resource-only update must NOT clear the stored `issuer` binding —
    /// the DCR-provenanced requesting pair is untouched, so its issuer
    /// binding stays valid.
    #[tokio::test]
    async fn endpoint_credentials_resource_only_update_preserves_issuer() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, tm) = test_state_with_token_manager("ep1", tmp.path(), None, None).await;

        let stored = DcrCredentials {
            client_id: "dcr-client".to_string(),
            client_secret: Some("dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 42,
            issuer: Some("https://as.example.com".to_string()),
            resource_client_id: None,
            resource_client_secret: None,
            registered_via_dcr: true,
        };
        tm.save_dcr("ep1", &stored).await.unwrap();

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "resource_client_id": "res-client",
                            "resource_client_secret": "res-secret",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let loaded = tm.load_dcr("ep1").await.unwrap().unwrap();
        assert!(
            loaded.registered_via_dcr,
            "resource-only update must preserve the DCR provenance flag"
        );
        assert_eq!(
            loaded.issuer.as_deref(),
            Some("https://as.example.com"),
            "resource-only update must preserve the existing issuer binding"
        );
    }

    // -----------------------------------------------------------------------
    // oauth_start: AS discovery when oauth_server_url is set
    // -----------------------------------------------------------------------

    /// Spawn a Router on 127.0.0.1:0 and return its base URL.
    async fn spawn_mock_as(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        // Tiny delay to let the server start accepting connections.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// Build the AS metadata `issuer` from the request `Host` header so a mock
    /// AS advertises an issuer matching its own origin, exactly like a real AS
    /// (RFC 8414 §2/§3.3). Discovery validates the advertised issuer against the
    /// probe origin, so hardcoding a foreign issuer would be (correctly) rejected.
    fn mock_issuer(headers: &axum::http::HeaderMap) -> String {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("127.0.0.1");
        format!("http://{host}")
    }

    /// Build a ManagementState wired for `oauth_start` against a mock AS.
    fn test_state_oauth_start(
        name: &str,
        oauth_server_url: &str,
        token_endpoint: Option<&str>,
    ) -> (ManagementState, Arc<OAuthFlowManager>) {
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let cfg = Config {
            relay: RelayConfig {
                machine_name: "test-machine".to_string(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: Some(true),
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints: vec![EndpointConfig {
                name: name.to_string(),
                description: None,
                tool_prefix: None,
                transport: Transport::Oauth,
                command: None,
                args: None,
                url: Some("http://127.0.0.1:9/mcp".to_string()),
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: Some(oauth_server_url.to_string()),
                client_id: Some("test-client".to_string()),
                client_secret: None,
                scopes: None,
                token_endpoint: token_endpoint.map(|s| s.to_string()),
                server_type_override: None,
                isolation: None,
                container_image: None,
                mounts: None,
                auth: None,
            }],
            profiles: None,
            organizations: Vec::new(),
        };
        let state = ManagementState {
            registry: Arc::new(AdapterRegistry::new()),
            config: Arc::new(RwLock::new(cfg)),
            start_time: Instant::now(),
            config_path: None,
            oauth_flow_manager: Some(flow_mgr.clone()),
            relay_port: 9400,
            oauth_adapter_inners: None,
            token_manager: None,
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
        };
        (state, flow_mgr)
    }

    /// Extract the `state` query parameter from an authorize URL.
    fn extract_state_param(authorize_url: &str) -> String {
        let url = url::Url::parse(authorize_url).expect("valid authorize URL");
        url.query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("authorize URL has state param")
    }

    // -----------------------------------------------------------------------
    // oauth_probe: add-time OAuth-capability probe (RFC 9728 → 8414)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oauth_probe_reports_supported_when_metadata_present() {
        // Bind first so the protected-resource metadata can point its
        // authorization_servers at this same mock origin.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());

        async fn protected_resource(State(base): State<String>) -> Json<Value> {
            Json(serde_json::json!({
                "resource": base,
                "authorization_servers": [base],
                "scopes_supported": ["read", "write"],
            }))
        }
        async fn auth_server(State(base): State<String>) -> Json<Value> {
            Json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                "token_endpoint": format!("{}/token", base),
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["read", "write"],
            }))
        }
        let router = Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource),
            )
            .route("/.well-known/oauth-authorization-server", get(auth_server))
            .with_state(base.clone());
        let _server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let state = test_state(vec![]).await;
        state.config.write().await.relay.allow_insecure_oauth = Some(true);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/oauth/probe")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({ "url": base }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["oauth_supported"], true);
        assert_eq!(body["authorization_server"], base);
        assert_eq!(
            body["scopes_supported"],
            serde_json::json!(["read", "write"])
        );
    }

    #[tokio::test]
    async fn oauth_probe_reports_unsupported_on_404() {
        // Mock server returns 404 for the well-known endpoints → discovery
        // fails and the probe must report oauth_supported:false as a 200.
        async fn not_found() -> StatusCode {
            StatusCode::NOT_FOUND
        }
        let router = Router::new()
            .route("/.well-known/oauth-protected-resource", get(not_found))
            .route("/.well-known/oauth-authorization-server", get(not_found));
        let (base_url, _server) = spawn_mock_as(router).await;

        let state = test_state(vec![]).await;
        state.config.write().await.relay.allow_insecure_oauth = Some(true);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/oauth/probe")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "url": base_url }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["oauth_supported"], false);
        assert!(body.get("authorization_server").is_none());
        assert!(body.get("scopes_supported").is_none());
    }

    #[tokio::test]
    async fn oauth_start_with_oauth_server_url_uses_discovery_when_available() {
        // Mock AS: serve real-looking endpoints at /.well-known/oauth-authorization-server.
        // Discovered URLs intentionally differ from the convention `{base}/authorize`
        // and `{base}/token`.
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            Json(serde_json::json!({
                "issuer": mock_issuer(&headers),
                "authorization_endpoint": "http://example.test/discovered-auth",
                "token_endpoint": "http://example.test/discovered-token",
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.starts_with("http://example.test/discovered-auth?"),
            "expected discovered authorization_endpoint, got: {}",
            authorize_url
        );
        assert_eq!(body["discovery"]["auth_server"], base_url);
    }

    #[tokio::test]
    async fn oauth_start_with_oauth_server_url_falls_back_to_convention_on_404() {
        // Mock AS: return 404 for the well-known. Discovery should fail and
        // oauth_start should fall back to the convention `{base}/authorize`.
        async fn not_found() -> StatusCode {
            StatusCode::NOT_FOUND
        }
        let router = Router::new().route("/.well-known/oauth-authorization-server", get(not_found));
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Flow still proceeds — fallback is transparent to the caller.
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let expected_prefix = format!("{}/authorize?", base_url);
        assert!(
            authorize_url.starts_with(&expected_prefix),
            "expected convention-constructed authorize URL `{}…`, got: {}",
            expected_prefix,
            authorize_url
        );
        // No discovery metadata on the fallback path.
        assert!(body.get("discovery").is_none() || body["discovery"].is_null());
    }

    #[test]
    fn google_authorization_endpoint_detection() {
        use crate::oauth::is_google_authorization_endpoint;
        assert!(is_google_authorization_endpoint(
            "https://accounts.google.com/o/oauth2/v2/auth"
        ));
        assert!(is_google_authorization_endpoint(
            "https://ACCOUNTS.GOOGLE.COM/o/oauth2/v2/auth"
        ));
        // Lookalike hosts must not match.
        assert!(!is_google_authorization_endpoint(
            "https://accounts.google.com.evil.test/auth"
        ));
        assert!(!is_google_authorization_endpoint(
            "https://auth.example.com/authorize"
        ));
        assert!(!is_google_authorization_endpoint("not a url"));
    }

    /// Mock AS whose metadata is served locally; the discovered
    /// authorization_endpoint is NOT Google, so no access_type=offline.
    async fn spawn_plain_as() -> (String, tokio::task::JoinHandle<()>) {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let base = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": format!("{}/authorize", base),
                "token_endpoint": format!("{}/token", base),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        spawn_mock_as(router).await
    }

    #[tokio::test]
    async fn oauth_start_force_consent_appends_prompt_consent() {
        let (base_url, _server) = spawn_plain_as().await;
        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start?force_consent=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.contains("&prompt=consent"),
            "force_consent=true must append prompt=consent, got: {}",
            authorize_url
        );
        // Non-Google AS: no access_type=offline.
        assert!(
            !authorize_url.contains("access_type=offline"),
            "non-Google AS must not get access_type=offline, got: {}",
            authorize_url
        );
    }

    #[tokio::test]
    async fn oauth_start_regular_has_no_prompt_consent() {
        // Regular Authorize / Re-authorize is unchanged: no forced consent.
        let (base_url, _server) = spawn_plain_as().await;
        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            !authorize_url.contains("prompt=consent"),
            "regular start must not force consent, got: {}",
            authorize_url
        );
    }

    #[tokio::test]
    async fn oauth_start_superseded_by_reset_during_discovery() {
        // The discovery-phase race Copilot flagged: a /oauth/start that began
        // BEFORE a reset is not pending yet while it does discovery, so
        // invalidate_endpoint cannot remove it. It must not later register a
        // flow under the bumped generation and hand out an authorize URL
        // without the reset's prompt=consent. The start must fail with 409
        // superseded_by_reset and leave no pending flow behind.
        // Two-step handshake, immune to lost wakeups on slow CI runners:
        // `entered_tx` tells the test the discovery request ARRIVED (the
        // start already sampled generation 0 — sampling precedes discovery),
        // and `gate.notify_one()` stores a permit, so the release completes
        // the handler's `notified().await` even if it parks afterwards.
        let gate = Arc::new(tokio::sync::Notify::new());
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let gate_srv = gate.clone();
        let well_known = move |headers: axum::http::HeaderMap| {
            let gate = gate_srv.clone();
            let entered_tx = entered_tx.clone();
            async move {
                // Signal arrival, then block discovery until the test has
                // performed the reset.
                let _ = entered_tx.send(());
                gate.notified().await;
                let base = mock_issuer(&headers);
                Json(serde_json::json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{}/authorize", base),
                    "token_endpoint": format!("{}/token", base),
                    "code_challenge_methods_supported": ["S256"],
                }))
            }
        };
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);

        // Start the pre-reset /oauth/start; it samples generation 0 at entry
        // and then parks inside discovery on the gate.
        let start_task = tokio::spawn(async move {
            app.oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        });
        // Wait until discovery is in flight (generation already sampled).
        tokio::time::timeout(std::time::Duration::from_secs(30), entered_rx.recv())
            .await
            .expect("discovery request never arrived")
            .expect("entered_tx dropped");

        // The reset lands mid-discovery: bump the generation (there is no
        // pending flow yet, so nothing is removed), then release discovery.
        assert_eq!(flow_mgr.invalidate_endpoint("ep1").await, 0);
        gate.notify_one();

        let resp = start_task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "superseded_by_reset");

        // No flow was registered for the superseded start.
        assert_eq!(
            flow_mgr.invalidate_endpoint("ep1").await,
            0,
            "superseded start must not leave a pending flow"
        );
    }

    #[tokio::test]
    async fn oauth_start_google_as_appends_access_type_offline() {
        // Mock AS metadata pointing the authorization_endpoint at Google's
        // AS: access_type=offline must be appended even WITHOUT
        // force_consent, so Google issues a refresh token.
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let base = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": "https://accounts.google.com/o/oauth2/v2/auth",
                "token_endpoint": format!("{}/token", base),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
            "expected Google authorization endpoint, got: {}",
            authorize_url
        );
        assert!(
            authorize_url.contains("&access_type=offline"),
            "Google AS must get access_type=offline, got: {}",
            authorize_url
        );
        assert!(!authorize_url.contains("prompt=consent"));

        // With force_consent both parameters are present.
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start?force_consent=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(authorize_url.contains("&prompt=consent"));
        assert!(authorize_url.contains("&access_type=offline"));
    }

    #[tokio::test]
    async fn oauth_start_with_oauth_server_url_errors_on_transient_discovery_failure() {
        // Bind a listener to reserve a port, then drop it so nothing is
        // listening: discovery hits a connection-refused (transient) error.
        // Unlike a 404 (metadata genuinely absent), a transient failure must
        // NOT fall back to the convention `{base}/authorize` — the guessed
        // URL would send the user to a dead page. Expect a structured
        // `discovery_unreachable` error instead.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let base_url = format!("http://127.0.0.1:{port}");

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "discovery_unreachable");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("Could not reach the OAuth server"),
            "detail should explain the connectivity failure, got: {}",
            body["detail"]
        );
        assert!(
            body.get("authorize_url").is_none(),
            "no authorize URL may be composed on a transient discovery failure"
        );
    }

    #[tokio::test]
    async fn oauth_start_with_oauth_server_url_errors_on_5xx_discovery_failure() {
        // Mock AS: 503 on every route (gateway/CDN in front of a down
        // origin). A 5xx is transient — the server likely does publish
        // metadata — so oauth_start must return `discovery_unreachable`
        // rather than compose the convention `{base}/authorize`.
        let router = Router::new().fallback(|| async { StatusCode::SERVICE_UNAVAILABLE });
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "discovery_unreachable");
        assert!(
            body.get("authorize_url").is_none(),
            "no authorize URL may be composed when the AS returns 5xx"
        );
    }

    #[tokio::test]
    async fn oauth_start_with_oauth_server_url_errors_on_non_404_discovery_failure() {
        // Mock AS: 403 on every route. A non-404, non-transient failure does
        // not establish that metadata is absent, so the convention fallback
        // must NOT run — expect a structured `discovery_failed` error.
        let router = Router::new().fallback(|| async { StatusCode::FORBIDDEN });
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "discovery_failed");
        assert!(
            body.get("authorize_url").is_none(),
            "no authorize URL may be composed on a non-404 discovery failure"
        );
    }

    #[tokio::test]
    async fn oauth_start_with_oauth_server_url_errors_when_s256_unsupported() {
        // Mock AS: metadata is served but only advertises the `plain` PKCE
        // method. S256NotSupported is neither transient nor 404-class, so
        // the convention fallback must NOT run.
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            Json(serde_json::json!({
                "issuer": mock_issuer(&headers),
                "authorization_endpoint": "http://example.test/authorize",
                "token_endpoint": "http://example.test/token",
                "code_challenge_methods_supported": ["plain"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        let (state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "discovery_failed");
        assert!(
            body["detail"].as_str().unwrap().contains("S256"),
            "detail should surface the S256 failure, got: {}",
            body["detail"]
        );
        assert!(
            body.get("authorize_url").is_none(),
            "no authorize URL may be composed when the AS lacks S256 support"
        );
    }

    #[tokio::test]
    async fn oauth_start_with_explicit_token_endpoint_overrides_discovery() {
        // Mock AS: discovery succeeds but advertises a token_endpoint that
        // differs from the operator-configured explicit override. The
        // override must win.
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            Json(serde_json::json!({
                "issuer": mock_issuer(&headers),
                "authorization_endpoint": "http://example.test/discovered-auth",
                "token_endpoint": "http://example.test/discovered-token",
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        let explicit_token = "http://example.test/explicit-token";
        let (state, flow_mgr) = test_state_oauth_start("ep1", &base_url, Some(explicit_token));
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        // Authorization endpoint still comes from discovery.
        assert!(
            authorize_url.starts_with("http://example.test/discovered-auth?"),
            "expected discovered authorization_endpoint, got: {}",
            authorize_url
        );
        // The registered pending flow must carry the EXPLICIT token endpoint.
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(flow.token_endpoint, explicit_token);
    }

    // -----------------------------------------------------------------------
    // PR #69 audit gap 2b: oauth_start with a trailing-slash oauth_server_url
    // must not produce a double-slash well-known URL during AS discovery.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oauth_start_with_trailing_slash_oauth_server_url_no_double_slash_discovery() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Counters for both possible request shapes. The trailing-slash
        // oauth_server_url must produce `/.well-known/...`; a buggy
        // concatenation would produce `//.well-known/...`.
        let well_known_hits = Arc::new(AtomicUsize::new(0));
        let bad_double_slash_hits = Arc::new(AtomicUsize::new(0));
        let wk = well_known_hits.clone();
        let bad = bad_double_slash_hits.clone();

        let well_known_handler = move |headers: axum::http::HeaderMap| {
            let wk = wk.clone();
            async move {
                wk.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({
                    "issuer": mock_issuer(&headers),
                    "authorization_endpoint": "http://example.test/discovered-auth",
                    "token_endpoint": "http://example.test/discovered-token",
                    "code_challenge_methods_supported": ["S256"],
                }))
            }
        };
        let bad_handler = move || {
            let bad = bad.clone();
            async move {
                bad.fetch_add(1, Ordering::SeqCst);
                StatusCode::IM_A_TEAPOT
            }
        };
        let router = Router::new()
            .route(
                "/.well-known/oauth-authorization-server",
                get(well_known_handler),
            )
            // A double-slash path would route to this matcher if the bug
            // re-appeared; bumping the counter makes the failure explicit.
            .route("//.well-known/oauth-authorization-server", get(bad_handler));
        let (base_url, _server) = spawn_mock_as(router).await;

        // Trailing slash, exactly like `https://accounts.google.com/`.
        let trailing = format!("{}/", base_url);
        let (state, flow_mgr) = test_state_oauth_start("ep1", &trailing, None);
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.starts_with("http://example.test/discovered-auth?"),
            "expected discovered authorization_endpoint, got: {}",
            authorize_url
        );
        // The discovered token endpoint must be registered with the pending
        // flow, proving discovery succeeded against the trailing-slash URL.
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(flow.token_endpoint, "http://example.test/discovered-token");
        // Exact counter assertions pin down the bug shape.
        assert_eq!(
            well_known_hits.load(Ordering::SeqCst),
            1,
            "well-known endpoint must be hit exactly once"
        );
        assert_eq!(
            bad_double_slash_hits.load(Ordering::SeqCst),
            0,
            "double-slash well-known path must NOT be hit"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: oauth_start must prefer the DCR-persisted client_secret
    // when TOML only has client_id (e.g. endpoints added via the desktop UI
    // that write client_id to TOML but client_secret only to the DCR file).
    // Without this, Google Drive re-auth fails with `client_secret is missing`.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oauth_start_prefers_dcr_client_secret_when_toml_only_has_client_id() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            Json(serde_json::json!({
                "issuer": mock_issuer(&headers),
                "authorization_endpoint": "http://example.test/authorize",
                "token_endpoint": "http://example.test/token",
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        // Build state with a TokenManager and an endpoint that has client_id
        // in TOML but no client_secret in TOML.
        let tmp = tempfile::tempdir().unwrap();
        let token_dir = tmp.path().to_path_buf();
        let token_manager = Arc::new(TokenManager::new(token_dir));

        // Pre-populate the DCR file: matching client_id, real client_secret.
        let creds = DcrCredentials {
            client_id: "from-toml".to_string(),
            client_secret: Some("from-dcr".to_string()),
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: None,
            ..Default::default()
        };
        token_manager.save_dcr("ep1", &creds).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        // Override the seeded client_id so it matches the DCR record.
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = Some("from-toml".to_string());
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager);

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(flow.client_id, "from-toml");
        assert_eq!(
            flow.client_secret,
            Some("from-dcr".to_string()),
            "oauth_start must load client_secret from the DCR file when TOML has only client_id"
        );
    }

    #[tokio::test]
    async fn oauth_start_falls_back_to_config_when_dcr_client_id_mismatches() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            Json(serde_json::json!({
                "issuer": mock_issuer(&headers),
                "authorization_endpoint": "http://example.test/authorize",
                "token_endpoint": "http://example.test/token",
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        let router =
            Router::new().route("/.well-known/oauth-authorization-server", get(well_known));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        // DCR record has a different client_id than the TOML config; the
        // DCR secret must NOT be paired with the mismatching TOML client_id.
        let creds = DcrCredentials {
            client_id: "stale-dcr-id".to_string(),
            client_secret: Some("stale-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: None,
            ..Default::default()
        };
        token_manager.save_dcr("ep1", &creds).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = Some("toml-id".to_string());
            cfg.endpoints[0].client_secret = Some("toml-secret".to_string());
        }
        state.token_manager = Some(token_manager);

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(flow.client_id, "toml-id");
        assert_eq!(flow.client_secret, Some("toml-secret".to_string()));
    }

    // -----------------------------------------------------------------------
    // Approach Part 2: interactive auth-start re-registers a fresh DCR client
    // when the persisted record is DCR-provenanced and a live
    // registration_endpoint is advertised. Manual credentials must never be
    // re-registered, and a failed re-registration must fall back to the
    // stored credentials so the flow still produces an authorize URL.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oauth_start_reregisters_when_persisted_creds_are_dcr_and_registration_endpoint_available(
    ) {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(serde_json::json!({
                "client_id": "fresh-dcr-client",
                "client_secret": "fresh-dcr-secret",
            }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        // Seed a per-endpoint MAS resource credential pair alongside the
        // stale DCR client so we can assert re-registration preserves it
        // (that pair is captured separately via POST /credentials and is a
        // distinct registration from the requesting client).
        let stored = DcrCredentials {
            client_id: "stale-dcr-client".to_string(),
            client_secret: Some("stale-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: Some(base_url.clone()),
            resource_client_id: Some("mas-resource-client".to_string()),
            resource_client_secret: Some("mas-resource-secret".to_string()),
            registered_via_dcr: true,
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = None;
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "fresh-dcr-client",
            "auth-start must use the freshly-registered client_id, not the stored one"
        );
        assert_eq!(flow.client_secret, Some("fresh-dcr-secret".to_string()));

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "fresh-dcr-client");
        assert_eq!(
            persisted.client_secret,
            Some("fresh-dcr-secret".to_string())
        );
        assert!(persisted.registered_via_dcr);
        assert_eq!(persisted.issuer.as_deref(), Some(base_url.as_str()));
        // The MAS resource credential pair is a distinct registration and
        // must survive re-registration of the requesting client.
        assert_eq!(
            persisted.resource_client_id.as_deref(),
            Some("mas-resource-client")
        );
        assert_eq!(
            persisted.resource_client_secret.as_deref(),
            Some("mas-resource-secret")
        );
    }

    #[tokio::test]
    async fn oauth_start_dcr_reregistration_falls_back_to_stored_creds_on_failure() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> (StatusCode, Json<Value>) {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({})),
            )
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        let stored = DcrCredentials {
            client_id: "stored-dcr-client".to_string(),
            client_secret: Some("stored-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 42,
            issuer: Some(base_url.clone()),
            registered_via_dcr: true,
            ..Default::default()
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = None;
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "DCR re-registration failure must fall back, not hard-fail the flow"
        );
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(flow.client_id, "stored-dcr-client");
        assert_eq!(flow.client_secret, Some("stored-dcr-secret".to_string()));

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "stored-dcr-client");
        assert_eq!(
            persisted.registered_at, 42,
            "failed re-registration must not overwrite the persisted record"
        );
    }

    #[tokio::test]
    async fn oauth_start_does_not_reregister_manual_persisted_credentials() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        // If the code path ever hits /register, the persisted record and the
        // flow would end up with this client_id — the asserts below would fail.
        async fn register() -> Json<Value> {
            Json(serde_json::json!({ "client_id": "should-not-be-used" }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        let stored = DcrCredentials {
            client_id: "manual-client".to_string(),
            client_secret: Some("manual-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 7,
            issuer: Some(base_url.clone()),
            registered_via_dcr: false,
            ..Default::default()
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = None;
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "manual-client",
            "manual credentials must be reused as-is; never re-registered"
        );
        assert_eq!(flow.client_secret, Some("manual-secret".to_string()));

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "manual-client");
        assert_eq!(persisted.registered_at, 7);
        assert!(!persisted.registered_via_dcr);
    }

    /// Regression for PR #130 Finding 1: setup commits used to persist the
    /// DCR `client_id`/`client_secret` into `config.toml` alongside the
    /// `.dcr.json` file (commit still stamps `client_id`, and legacy configs
    /// carry both), so `config_client_id.is_some()` is true for such
    /// endpoints. If the config-branch is checked first, the DCR-provenanced
    /// re-registration heal path is unreachable and stale server-side
    /// registrations loop forever. The resolution must check DCR-provenanced
    /// re-registration BEFORE the config branch and mint a fresh client_id.
    #[tokio::test]
    async fn oauth_start_reregisters_dcr_record_even_when_config_carries_stale_client_id() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(serde_json::json!({
                "client_id": "fresh-dcr-client",
                "client_secret": "fresh-dcr-secret",
            }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        // The DCR record and `config.toml` BOTH carry the same stale
        // `client_id` — commit stamps `client_id` into `config.toml`
        // (secrets stay in the DCR store only), and legacy configs from
        // older commits carry the full pair. The record is
        // DCR-provenanced so the RFC 7591 heal path applies.
        let stored = DcrCredentials {
            client_id: "stale-dcr-client".to_string(),
            client_secret: Some("stale-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: Some(base_url.clone()),
            registered_via_dcr: true,
            ..Default::default()
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = Some("stale-dcr-client".to_string());
            cfg.endpoints[0].client_secret = Some("stale-dcr-secret".to_string());
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "fresh-dcr-client",
            "config.toml carrying the same stale DCR client_id must NOT block \
             the DCR-provenanced re-registration heal path"
        );
        assert_eq!(flow.client_secret, Some("fresh-dcr-secret".to_string()));

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "fresh-dcr-client");
        assert!(persisted.registered_via_dcr);
    }

    /// Regression for PR #130 round-2 finding R2: after a token-endpoint
    /// `invalid_client` self-heal on a pure-DCR record the on-disk record
    /// keeps `client_id=""` / `client_secret=None` with
    /// `registered_via_dcr = true` (rather than being deleted outright).
    /// This test asserts that the next interactive Authorize on an endpoint
    /// whose `config.toml` still carries the previous DCR `client_id`
    /// still takes the DCR-provenanced re-registration heal path and
    /// mints a fresh `client_id` — never falls back to the stale
    /// config value.
    #[tokio::test]
    async fn oauth_start_after_self_heal_reregisters_pure_dcr_stub_ignoring_stale_config_client_id()
    {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(serde_json::json!({
                "client_id": "fresh-dcr-client",
                "client_secret": "fresh-dcr-secret",
            }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        // Post-self-heal stub: cleared requesting pair, cleared issuer,
        // provenance flag retained. This is exactly what
        // `TokenManager::clear_dcr_requesting_client` leaves on disk after
        // a token-endpoint `invalid_client`.
        let post_heal = DcrCredentials {
            client_id: String::new(),
            client_secret: None,
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: None,
            resource_client_id: None,
            resource_client_secret: None,
            registered_via_dcr: true,
        };
        token_manager.save_dcr("ep1", &post_heal).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = Some("stale-dcr-client".to_string());
            cfg.endpoints[0].client_secret = Some("stale-dcr-secret".to_string());
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "fresh-dcr-client",
            "post-self-heal stub with registered_via_dcr=true must still \
             drive DCR re-registration; the stale config.toml client_id \
             must NOT be used"
        );
        assert_eq!(flow.client_secret, Some("fresh-dcr-secret".to_string()));

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "fresh-dcr-client");
        assert!(persisted.registered_via_dcr);
    }

    /// Regression for PR #130 round-2 finding R3: after a token-endpoint
    /// `invalid_client` self-heal on a MIXED record (requesting pair
    /// alongside an operator-set MAS resource pair), the on-disk record
    /// keeps `client_id=""` / `client_secret=None` / `resource_*` intact
    /// with `registered_via_dcr = true`. The next interactive Authorize
    /// on an endpoint whose `config.toml` still carries the previous
    /// DCR `client_id` must take the DCR-provenanced re-registration
    /// heal path (mint a fresh id) while preserving the resource pair.
    #[tokio::test]
    async fn oauth_start_after_self_heal_reregisters_mixed_record_and_preserves_resource_pair() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(serde_json::json!({
                "client_id": "fresh-dcr-client",
                "client_secret": "fresh-dcr-secret",
            }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        // Post-self-heal state on a mixed record: requesting pair cleared,
        // MAS resource pair retained, provenance flag retained.
        let post_heal = DcrCredentials {
            client_id: String::new(),
            client_secret: None,
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: None,
            resource_client_id: Some("mas-resource-client".to_string()),
            resource_client_secret: Some("mas-resource-secret".to_string()),
            registered_via_dcr: true,
        };
        token_manager.save_dcr("ep1", &post_heal).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = Some("stale-dcr-client".to_string());
            cfg.endpoints[0].client_secret = Some("stale-dcr-secret".to_string());
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "fresh-dcr-client",
            "post-self-heal mixed stub must still drive DCR re-registration \
             (registered_via_dcr survives the clear); the stale config.toml \
             client_id must NOT be used"
        );

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "fresh-dcr-client");
        assert!(persisted.registered_via_dcr);
        assert_eq!(
            persisted.resource_client_id.as_deref(),
            Some("mas-resource-client"),
            "operator-set MAS resource pair must survive self-heal + re-registration"
        );
        assert_eq!(
            persisted.resource_client_secret.as_deref(),
            Some("mas-resource-secret")
        );
    }

    /// Regression for PR #130 round-2 finding R4: a legacy `.dcr.json`
    /// record (one with no `registered_via_dcr` field, which deserializes
    /// as `false`) must survive an issuer change on the AS. Auto-discarding
    /// it and silently re-registering would break the "manual/legacy
    /// credentials survive" promise — the issuer-mismatch discard only
    /// applies to DCR-provenanced records.
    #[tokio::test]
    async fn oauth_start_preserves_legacy_record_across_issuer_change() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        // If the code path ever falls through to /register the flow would
        // end up with this id — the assertion below would fail.
        async fn register() -> Json<Value> {
            Json(serde_json::json!({ "client_id": "should-not-be-used" }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));

        // Legacy record: bound to a DIFFERENT issuer than the currently
        // discovered one, `registered_via_dcr = false` (this is what
        // pre-provenance-flag `.dcr.json` files deserialize as).
        let stored = DcrCredentials {
            client_id: "legacy-client".to_string(),
            client_secret: Some("legacy-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 42,
            issuer: Some("https://old-as.example.com".to_string()),
            resource_client_id: None,
            resource_client_secret: None,
            registered_via_dcr: false,
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = None;
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "legacy-client",
            "legacy records (registered_via_dcr=false) must survive an issuer \
             change; only DCR-provenanced records participate in the \
             issuer-mismatch discard"
        );
        assert_eq!(flow.client_secret, Some("legacy-secret".to_string()));

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "legacy-client");
        assert_eq!(persisted.registered_at, 42);
        assert!(!persisted.registered_via_dcr);
    }

    /// Regression for PR #130 round-3 finding R3-4: when a
    /// post-self-heal DCR tombstone (`client_id=""`,
    /// `registered_via_dcr=true`) triggers the re-registration heal path
    /// and the registration endpoint is temporarily unreachable, the
    /// auth-start handler must NOT fall back to the empty stored id
    /// (which would yield an unusable `client_id=` authorize URL). It
    /// must surface a clear `503 dcr_registration_unavailable` error
    /// instead, leaving the tombstone in place for retry.
    #[tokio::test]
    async fn oauth_start_dcr_failure_on_tombstone_returns_error_not_empty_authorize_url() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        // Simulate the AS's registration endpoint being temporarily down.
        async fn register() -> (StatusCode, Json<Value>) {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "server_error"})),
            )
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let tombstone = DcrCredentials {
            client_id: String::new(),
            client_secret: None,
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: None,
            resource_client_id: None,
            resource_client_secret: None,
            registered_via_dcr: true,
        };
        token_manager.save_dcr("ep1", &tombstone).await.unwrap();

        let (mut state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = Some("stale-dcr-client".to_string());
            cfg.endpoints[0].client_secret = Some("stale-dcr-secret".to_string());
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "DCR failure on a tombstone must not fall back to the empty stored id"
        );
        let body = body_json(resp).await;
        assert_eq!(body["error"], "dcr_registration_unavailable");

        // Tombstone is preserved on disk so a retry can re-attempt registration.
        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert!(persisted.client_id.is_empty());
        assert!(persisted.registered_via_dcr);
    }

    /// Round-4 finding R4-1: when the AS issuer changes, the DCR
    /// requesting pair (`client_id`/`client_secret`/`issuer`) is
    /// invalidated so `oauth_start` re-registers via RFC 7591, but the
    /// operator-set `resource_client_id`/`resource_client_secret` pair is
    /// a distinct MAS registration and MUST be carried through the
    /// invalidation. Prior to the fix the mismatched record was collapsed
    /// to `None`, and the fresh-registration branch built the new record
    /// with `..Default::default()`, silently dropping the resource pair.
    #[tokio::test]
    async fn oauth_start_issuer_mismatch_preserves_resource_pair() {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(serde_json::json!({
                "client_id": "fresh-dcr-client",
                "client_secret": "fresh-dcr-secret",
            }))
        }
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register));
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        // Persisted DCR record bound to a DIFFERENT issuer than the AS
        // will now advertise, alongside an operator-set MAS resource pair.
        let stored = DcrCredentials {
            client_id: "stale-dcr-client".to_string(),
            client_secret: Some("stale-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: Some("https://old-idp.example.com".to_string()),
            resource_client_id: Some("mas-resource-client".to_string()),
            resource_client_secret: Some("mas-resource-secret".to_string()),
            registered_via_dcr: true,
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = None;
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending flow was registered");
        assert_eq!(
            flow.client_id, "fresh-dcr-client",
            "issuer mismatch must invalidate the requesting pair and drive re-registration"
        );

        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted.client_id, "fresh-dcr-client");
        assert!(persisted.registered_via_dcr);
        assert_eq!(persisted.issuer.as_deref(), Some(base_url.as_str()));
        assert_eq!(
            persisted.resource_client_id.as_deref(),
            Some("mas-resource-client"),
            "operator-set MAS resource pair must survive issuer-mismatch invalidation"
        );
        assert_eq!(
            persisted.resource_client_secret.as_deref(),
            Some("mas-resource-secret")
        );
    }

    /// Round-4 finding R4-4 + round-5 finding R5-2: the persist of a
    /// freshly-registered DCR requesting client uses `update_dcr`
    /// (compare-and-update) so a concurrent operator write that lands
    /// between our snapshot and this save — for example, a
    /// `POST /credentials` that rotated to manual credentials — SURVIVES
    /// intact instead of being clobbered (R4-4); AND the auth-start
    /// handler refuses to continue with the unpersisted fresh DCR pair
    /// on compare-failure, returning a `CONFLICT` superseded response
    /// so no callback can install credentials into the running adapter
    /// that never made it to disk (R5-2). This test simulates the race
    /// by letting the mock `/register` handler block until the test
    /// manually rotates the persisted record via `save_dcr` to a
    /// manual-provenance record, then unblocks.
    #[tokio::test]
    async fn oauth_start_reregister_save_does_not_clobber_concurrent_manual_rotation() {
        use tokio::sync::oneshot;

        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        // `/register` waits for the test to signal (via `tx_ready`) that
        // the operator's concurrent manual write has landed, then returns
        // the fresh DCR response. This deterministically interleaves the
        // manual write between the caller's snapshot and its save.
        struct RegState {
            ready_tx: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
            proceed_rx: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
        }
        async fn register(
            axum::extract::State(s): axum::extract::State<Arc<RegState>>,
        ) -> Json<Value> {
            if let Some(tx) = s.ready_tx.lock().await.take() {
                let _ = tx.send(());
            }
            if let Some(rx) = s.proceed_rx.lock().await.take() {
                let _ = rx.await;
            }
            Json(serde_json::json!({
                "client_id": "fresh-dcr-client",
                "client_secret": "fresh-dcr-secret",
            }))
        }
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let (proceed_tx, proceed_rx) = oneshot::channel::<()>();
        let reg_state = Arc::new(RegState {
            ready_tx: tokio::sync::Mutex::new(Some(ready_tx)),
            proceed_rx: tokio::sync::Mutex::new(Some(proceed_rx)),
        });
        let router = Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", post(register))
            .with_state(reg_state);
        let (base_url, _server) = spawn_mock_as(router).await;

        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let stored = DcrCredentials {
            client_id: "stale-dcr-client".to_string(),
            client_secret: Some("stale-dcr-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 0,
            issuer: Some(base_url.clone()),
            resource_client_id: None,
            resource_client_secret: None,
            registered_via_dcr: true,
        };
        token_manager.save_dcr("ep1", &stored).await.unwrap();

        let (mut state, _flow_mgr) = test_state_oauth_start("ep1", &base_url, None);
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].client_id = None;
            cfg.endpoints[0].client_secret = None;
        }
        state.token_manager = Some(token_manager.clone());

        let app = management_routes(state);
        let start_fut = tokio::spawn(async move {
            app.oneshot(
                Request::post("/api/endpoints/ep1/oauth/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        });

        // Wait until the register handler is entered (proves the caller
        // already read its snapshot of the DCR file) before rotating.
        ready_rx.await.expect("register endpoint reached");
        let manual = DcrCredentials {
            client_id: "operator-manual-client".to_string(),
            client_secret: Some("operator-manual-secret".to_string()),
            client_secret_expires_at: 0,
            registered_at: 1_700_000_000,
            issuer: None,
            resource_client_id: None,
            resource_client_secret: None,
            registered_via_dcr: false,
        };
        token_manager.save_dcr("ep1", &manual).await.unwrap();
        // Now let the mock AS finish returning the fresh DCR response.
        proceed_tx.send(()).unwrap();

        let resp = start_fut.await.unwrap();
        // R5-2: the auth-start handler MUST refuse to continue with the
        // unpersisted fresh DCR pair when the on-disk record was rotated
        // out from under it. Otherwise a successful callback would install
        // credentials on the running adapter that never made it to disk,
        // bypassing the R5-3 callback guard (the manual write has
        // `registered_via_dcr = false`, so the guard's provenance check
        // never fires).
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["error"], "auth_start_superseded");

        // The persisted record must be the operator's manual write, not
        // the concurrent DCR re-registration result.
        let persisted = token_manager.load_dcr("ep1").await.unwrap().unwrap();
        assert_eq!(persisted, manual, "concurrent manual rotation must survive");
    }

    // -----------------------------------------------------------------------
    // PR #69 audit gap 4: allow_insecure_oauth threading through
    // restart_endpoint and reload_config (mirrors the existing watcher tests
    // for apply_diff / apply_diff_graceful).
    // -----------------------------------------------------------------------

    /// Wait for `name` to appear in the OAuthAdapterInners map, returning the
    /// shared inner. Panics on timeout.
    async fn wait_for_inner(
        inners: &OAuthAdapterInners,
        name: &str,
    ) -> Arc<crate::adapter::oauth::OAuthAdapterInner> {
        let stop = Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(inner) = inners.read().await.get(name).cloned() {
                return inner;
            }
            if Instant::now() >= stop {
                panic!("OAuthAdapterInner for `{}` was never registered", name);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Build a config containing a single OAuth endpoint with the given
    /// `allow_insecure_oauth` toggle. Used by the threading tests below.
    fn oauth_config_with_insecure(name: &str, allow_insecure_oauth: bool) -> Config {
        Config {
            relay: RelayConfig {
                machine_name: "test-machine".to_string(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: Some(allow_insecure_oauth),
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints: vec![EndpointConfig {
                name: name.to_string(),
                description: None,
                tool_prefix: None,
                transport: Transport::Oauth,
                command: None,
                args: None,
                url: Some("http://127.0.0.1:5000/mcp".to_string()),
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: Some("http://127.0.0.1:5001".to_string()),
                client_id: Some("client123".to_string()),
                client_secret: None,
                scopes: None,
                token_endpoint: None,
                server_type_override: None,
                isolation: None,
                container_image: None,
                mounts: None,
                auth: None,
            }],
            profiles: None,
            organizations: Vec::new(),
        }
    }

    #[tokio::test]
    async fn restart_endpoint_threads_allow_insecure_oauth_to_rebuilt_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
        let oauth_inners: OAuthAdapterInners =
            Arc::new(RwLock::new(std::collections::HashMap::new()));

        // Seed the registry with a placeholder MockAdapter under the OAuth
        // endpoint's name. restart_endpoint will replace it via
        // watcher::create_adapter, which reads allow_insecure_oauth from
        // state.config.relay.
        let registry = AdapterRegistry::new();
        registry
            .register(
                "oauth_restart_ep".to_string(),
                Box::new(MockAdapter::healthy_with_tools(vec![])),
                "oauth".to_string(),
                None,
                Some("oauth_restart_ep".to_string()),
            )
            .await;

        let state = ManagementState {
            registry: Arc::new(registry),
            config: Arc::new(RwLock::new(oauth_config_with_insecure(
                "oauth_restart_ep",
                true,
            ))),
            start_time: Instant::now(),
            config_path: None,
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: Some(oauth_inners.clone()),
            token_manager: Some(token_manager),
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
        };
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/endpoints/oauth_restart_ep/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let inner = wait_for_inner(&oauth_inners, "oauth_restart_ep").await;
        assert!(
            inner.config.allow_insecure_oauth,
            "restart_endpoint must thread allow_insecure_oauth=true into the rebuilt OAuthAdapterConfig"
        );
    }

    #[tokio::test]
    async fn reload_config_threads_allow_insecure_oauth_to_new_adapter() {
        // Write a config file on disk whose [relay] section has
        // allow_insecure_oauth = true and a single OAuth endpoint. The
        // in-memory baseline starts with NO endpoints, so the reload sees
        // the OAuth endpoint as "added" and routes it through
        // apply_diff_graceful -> create_adapter with the flag.
        let tmp = tempfile::tempdir().unwrap();
        let token_dir = tmp.path().join("tokens");
        std::fs::create_dir_all(&token_dir).unwrap();
        let config_path = tmp.path().join("config.toml");
        let toml = r#"
[relay]
machine_name = "test-machine"
allow_insecure_oauth = true

[[endpoints]]
name = "oauth_reload_ep"
transport = "oauth"
url = "http://127.0.0.1:5000/mcp"
oauth_server_url = "http://127.0.0.1:5001"
client_id = "client123"
"#;
        std::fs::write(&config_path, toml).unwrap();

        let token_manager = Arc::new(TokenManager::new(token_dir));
        let oauth_inners: OAuthAdapterInners =
            Arc::new(RwLock::new(std::collections::HashMap::new()));

        let baseline = Config {
            relay: RelayConfig {
                machine_name: "test-machine".to_string(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: Some(false),
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints: vec![],
            profiles: None,
            organizations: Vec::new(),
        };
        let state = ManagementState {
            registry: Arc::new(AdapterRegistry::new()),
            config: Arc::new(RwLock::new(baseline)),
            start_time: Instant::now(),
            config_path: Some(config_path),
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: Some(oauth_inners.clone()),
            token_manager: Some(token_manager),
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
        };
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

        let inner = wait_for_inner(&oauth_inners, "oauth_reload_ep").await;
        assert!(
            inner.config.allow_insecure_oauth,
            "reload_config must thread allow_insecure_oauth=true from the on-disk config into the new OAuthAdapterConfig"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_toml_written_with_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        crate::config::write_config_file(&path, "# placeholder").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "Expected 0600, got {:o}", mode & 0o777);
    }

    // ---------------------------------------------------------------------
    // R4.A — Profile CRUD + endpoint membership (test matrix #21, #22)
    // ---------------------------------------------------------------------

    fn profiles_test_config(endpoint_names: &[&str]) -> Config {
        Config {
            relay: RelayConfig {
                machine_name: "test".into(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: None,
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints: endpoint_names
                .iter()
                .map(|n| EndpointConfig {
                    name: (*n).to_string(),
                    description: None,
                    tool_prefix: None,
                    transport: Transport::Stdio,
                    command: Some("echo".into()),
                    args: None,
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
                    isolation: None,
                    container_image: None,
                    mounts: None,
                    auth: None,
                })
                .collect(),
            profiles: None,
            organizations: Vec::new(),
        }
    }

    /// Build a `ManagementState` wired to a real on-disk `config.toml` and a
    /// live `ProfileRegistry`, plus `endpoint_names` worth of registered
    /// mock adapters. Returns the state and the path to the config file so
    /// tests can assert TOML writeback.
    async fn profiles_test_state(
        endpoint_names: &[&str],
        config_file: &std::path::Path,
    ) -> ManagementState {
        let registry = AdapterRegistry::new();
        for name in endpoint_names {
            registry
                .register(
                    (*name).to_string(),
                    Box::new(MockAdapter::healthy_with_tools(vec![ToolInfo {
                        name: format!("{}_tool", name),
                        description: None,
                        input_schema: serde_json::json!({}),
                        annotations: None,
                        ..Default::default()
                    }])),
                    "stdio".to_string(),
                    None,
                    Some((*name).to_string()),
                )
                .await;
        }
        let registry_arc = Arc::new(registry);
        let cfg = profiles_test_config(endpoint_names);
        // Seed the on-disk file so writeback round-trips through a real
        // TOML document and we can verify formatting.
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(config_file, toml_str).unwrap();

        let profile_registry = Arc::new(ProfileRegistry::new((*registry_arc).clone()));
        profile_registry.rebuild(&[]).await;

        ManagementState {
            registry: registry_arc,
            config: Arc::new(RwLock::new(cfg)),
            start_time: Instant::now(),
            config_path: Some(config_file.to_path_buf()),
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: None,
            token_manager: None,
            setup_manager: None,
            profile_registry: Some(profile_registry),
            event_bus: None,
        }
    }

    /// Matrix row #21 — Management API CRUD round trip.
    ///
    /// Create → list → get → update → delete, asserting status codes, JSON
    /// shapes, TOML writeback, and that the live `ProfileRegistry`
    /// reflects each mutation.
    #[tokio::test]
    async fn management_profiles_full_crud_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = profiles_test_state(&["gmail", "linear", "todoist"], &config_file).await;
        let profile_registry = state.profile_registry.clone().unwrap();
        let app = management_routes(state);

        // ---- CREATE ----
        let create_body = serde_json::json!({
            "name": "Work",
            "path": "work",
            "endpoints": ["gmail", "linear"],
            "js_execution": true,
            "toon_output": true,
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/profiles")
                    .header("content-type", "application/json")
                    .body(Body::from(create_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "Work");
        assert_eq!(body["path"], "work");
        assert_eq!(body["endpoint_count"], 2);
        assert_eq!(body["tool_count"], 2); // gmail_tool + linear_tool
        assert_eq!(body["js_execution"], true);
        assert_eq!(body["toon_output"], true);
        // Writeback: config.toml now contains the profile.
        let on_disk = std::fs::read_to_string(&config_file).unwrap();
        assert!(on_disk.contains("[[profiles]]"));
        assert!(on_disk.contains("name = \"Work\""));
        assert!(on_disk.contains("path = \"work\""));
        // Live registry sees the new profile.
        assert!(profile_registry.get("work").await.is_some());

        // ---- LIST ----
        let resp = app
            .clone()
            .oneshot(Request::get("/api/profiles").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "work");

        // ---- GET (case-insensitive path lookup) ----
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/profiles/WORK")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "Work");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "profile catalog must contain 2 tools");
        let tool_names: std::collections::HashSet<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert!(tool_names.contains("gmail__gmail_tool"));
        assert!(tool_names.contains("linear__linear_tool"));

        // ---- UPDATE (rename + change endpoints) ----
        let update_body = serde_json::json!({
            "name": "Daily",
            "path": "daily",
            "endpoints": ["gmail", "linear", "todoist"],
            "js_execution": false,
            "toon_output": false,
        });
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/profiles/work")
                    .header("content-type", "application/json")
                    .body(Body::from(update_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "Daily");
        assert_eq!(body["path"], "daily");
        assert_eq!(body["endpoint_count"], 3);
        assert_eq!(body["js_execution"], false);
        assert!(profile_registry.get("work").await.is_none());
        assert!(profile_registry.get("daily").await.is_some());

        // ---- DELETE ----
        let resp = app
            .clone()
            .oneshot(
                Request::delete("/api/profiles/daily")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(profile_registry.get("daily").await.is_none());
        let on_disk = std::fs::read_to_string(&config_file).unwrap();
        assert!(
            !on_disk.contains("[[profiles]]"),
            "TOML must drop the [[profiles]] block when empty, got:\n{}",
            on_disk
        );

        // ---- LIST is now empty ----
        let resp = app
            .clone()
            .oneshot(Request::get("/api/profiles").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    /// Matrix row #22 — `GET /api/endpoints/{name}/profiles` returns the
    /// list of profile paths containing the endpoint.
    #[tokio::test]
    async fn management_endpoint_profiles_membership() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = profiles_test_state(&["gmail", "linear", "todoist"], &config_file).await;
        let app = management_routes(state);

        // Seed two profiles via the API: gmail is in both, todoist in none.
        for (name, path, endpoints) in [
            ("Work", "work", vec!["gmail", "linear"]),
            ("Personal", "personal", vec!["gmail"]),
        ] {
            let body = serde_json::json!({
                "name": name,
                "path": path,
                "endpoints": endpoints,
                "js_execution": false,
                "toon_output": true,
            });
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/profiles")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        // gmail → both profiles, sorted.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/endpoints/gmail/profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body["profiles"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "personal");
        assert_eq!(arr[1], "work");

        // linear → only "work".
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/endpoints/linear/profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert_eq!(body["profiles"].as_array().unwrap().len(), 1);
        assert_eq!(body["profiles"][0], "work");

        // todoist → no profiles.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/endpoints/todoist/profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert!(body["profiles"].as_array().unwrap().is_empty());

        // Unknown endpoint → 404.
        let resp = app
            .oneshot(
                Request::get("/api/endpoints/missing/profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Validation: `POST /api/profiles` rejects invalid paths (regex +
    /// reserved-name list — spec §2.3) and unknown endpoint references with
    /// 4xx responses, and never writes the bad payload to disk.
    #[tokio::test]
    async fn management_profiles_post_rejects_invalid_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = profiles_test_state(&["gmail"], &config_file).await;
        let app = management_routes(state);
        let before = std::fs::read_to_string(&config_file).unwrap();

        let cases: &[(&str, serde_json::Value, StatusCode)] = &[
            (
                "empty name",
                serde_json::json!({
                    "name": "", "path": "work", "endpoints": [],
                    "js_execution": false, "toon_output": true,
                }),
                StatusCode::BAD_REQUEST,
            ),
            (
                "reserved path",
                serde_json::json!({
                    "name": "X", "path": "sse", "endpoints": [],
                    "js_execution": false, "toon_output": true,
                }),
                StatusCode::BAD_REQUEST,
            ),
            (
                "invalid path characters",
                serde_json::json!({
                    "name": "X", "path": "with space", "endpoints": [],
                    "js_execution": false, "toon_output": true,
                }),
                StatusCode::BAD_REQUEST,
            ),
            (
                "unknown endpoint reference",
                serde_json::json!({
                    "name": "X", "path": "x", "endpoints": ["nonexistent"],
                    "js_execution": false, "toon_output": true,
                }),
                StatusCode::BAD_REQUEST,
            ),
        ];
        for (label, body, want_status) in cases {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/profiles")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                *want_status,
                "case '{}' should reject with {}",
                label,
                want_status
            );
        }
        // None of the rejected payloads should have touched the file.
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(
            before, after,
            "rejected POSTs must not write to config.toml"
        );
    }

    /// `PUT /api/profiles/{path}` on a missing profile returns 404, and a
    /// rename to a path that collides with another profile returns 409.
    #[tokio::test]
    async fn management_profiles_put_404_and_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = profiles_test_state(&["gmail"], &config_file).await;
        let app = management_routes(state);

        // 404 on update of nonexistent profile.
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/profiles/ghost")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Ghost",
                            "path": "ghost",
                            "endpoints": [],
                            "js_execution": false,
                            "toon_output": true,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Seed two profiles, then try to rename one onto the other's path.
        for (name, path) in [("A", "alpha"), ("B", "bravo")] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/profiles")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "name": name,
                                "path": path,
                                "endpoints": [],
                                "js_execution": false,
                                "toon_output": true,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
        let resp = app
            .oneshot(
                Request::put("/api/profiles/bravo")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "B",
                            "path": "ALPHA",
                            "endpoints": [],
                            "js_execution": false,
                            "toon_output": true,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "rename onto existing path must 409 (case-insensitive)"
        );
    }

    /// `POST /api/profiles` with a body that omits (or nulls out)
    /// `js_execution` / `toon_output` is rejected by the JSON extractor
    /// with a 4xx, and never touches `config.toml` on disk.
    #[tokio::test]
    async fn management_profiles_post_rejects_missing_js_toon_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = profiles_test_state(&["gmail"], &config_file).await;
        let app = management_routes(state);
        let before = std::fs::read_to_string(&config_file).unwrap();

        let cases: &[(&str, serde_json::Value)] = &[
            (
                "missing js_execution",
                serde_json::json!({
                    "name": "Work", "path": "work", "endpoints": [],
                    "toon_output": true,
                }),
            ),
            (
                "missing toon_output",
                serde_json::json!({
                    "name": "Work", "path": "work", "endpoints": [],
                    "js_execution": false,
                }),
            ),
            (
                "null js_execution",
                serde_json::json!({
                    "name": "Work", "path": "work", "endpoints": [],
                    "js_execution": null, "toon_output": true,
                }),
            ),
            (
                "null toon_output",
                serde_json::json!({
                    "name": "Work", "path": "work", "endpoints": [],
                    "js_execution": false, "toon_output": null,
                }),
            ),
        ];
        for (label, body) in cases {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/profiles")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let s = resp.status();
            assert!(
                s.is_client_error(),
                "case '{}' should reject with a 4xx, got {}",
                label,
                s
            );
        }
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(
            before, after,
            "rejected POSTs must not write to config.toml"
        );
    }

    // ---------------------------------------------------------------------
    // Endpoint CRUD (issue #82) — POST /api/endpoints, PUT /api/endpoints/{name}
    // ---------------------------------------------------------------------

    /// Build a `ManagementState` wired to a real on-disk `config.toml`.
    /// Unlike `profiles_test_state` this seeds the registry with *no*
    /// adapters so we can assert that `POST /api/endpoints` registers a
    /// new one inline (without waiting on the file watcher). The seeded
    /// TOML contains a single stdio endpoint so update tests have
    /// something to mutate.
    async fn endpoints_test_state(config_file: &std::path::Path) -> ManagementState {
        let registry = AdapterRegistry::new();
        // Seed an "existing" entry the on-disk TOML will also reflect so
        // update / rename tests work against a real adapter.
        registry
            .register(
                "existing".to_string(),
                Box::new(MockAdapter::healthy_with_tools(vec![])),
                "stdio".to_string(),
                None,
                Some("existing".to_string()),
            )
            .await;
        let registry_arc = Arc::new(registry);
        let cfg = Config {
            relay: RelayConfig {
                machine_name: "test".into(),
                local_js_execution: None,
                token_dir: None,
                allow_insecure_oauth: None,
                toon_output: None,
                startup_init_timeout_secs: None,
                session_identity_max_sessions: None,
                validate_inputs: None,
                observability: crate::config::ObservabilityConfig::default(),
                log_retention_days: None,
                write_dirs: None,
            },
            endpoints: vec![EndpointConfig {
                name: "existing".to_string(),
                description: None,
                tool_prefix: None,
                transport: Transport::Stdio,
                command: Some("echo".into()),
                args: None,
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
                isolation: None,
                container_image: None,
                mounts: None,
                auth: None,
            }],
            profiles: None,
            organizations: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(config_file, toml_str).unwrap();
        ManagementState {
            registry: registry_arc,
            config: Arc::new(RwLock::new(cfg)),
            start_time: Instant::now(),
            config_path: Some(config_file.to_path_buf()),
            oauth_flow_manager: None,
            relay_port: 9400,
            oauth_adapter_inners: None,
            token_manager: None,
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
        }
    }

    #[tokio::test]
    async fn endpoint_create_happy_path_and_visible_in_list() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "newstdio",
            "transport": "stdio",
            "command": "echo",
            "args": ["hi"],
            "description": "a new endpoint",
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let summary = body_json(resp).await;
        assert_eq!(summary["name"], "newstdio");
        assert_eq!(summary["transport"], "stdio");
        assert_eq!(summary["command"], "echo");
        assert!(summary.get("client_secret").is_none());

        // TOML writeback contains the new entry.
        let on_disk = std::fs::read_to_string(&config_file).unwrap();
        assert!(on_disk.contains("name = \"newstdio\""));
        assert!(on_disk.contains("command = \"echo\""));

        // The new endpoint is visible in GET /api/endpoints without a
        // separate /api/config/reload call.
        let resp = app
            .clone()
            .oneshot(Request::get("/api/endpoints").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = body_json(resp).await;
        let names: std::collections::HashSet<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap_or(""))
            .collect();
        assert!(
            names.contains("newstdio"),
            "newly-created endpoint must appear in GET /api/endpoints, got {:?}",
            names
        );
    }

    #[tokio::test]
    async fn endpoint_create_rejects_duplicate_name() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let before = std::fs::read_to_string(&config_file).unwrap();
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "existing",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .oneshot(
                Request::post("/api/endpoints")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(
            before, after,
            "rejected create must not write to config.toml"
        );
    }

    /// A name reserved by a live OAuth setup session is refused by the
    /// regular create and rename APIs with 409, so a mid-setup name cannot
    /// be taken out from under the session's eventual commit.
    #[tokio::test]
    async fn endpoint_create_and_rename_reject_setup_reserved_name() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let mut state = endpoints_test_state(&config_file).await;
        let setup_mgr = Arc::new(OAuthSetupManager::new());
        let session_id = setup_mgr
            .create_session(
                "insetup".into(),
                "https://mcp.example.com".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        state.setup_manager = Some(setup_mgr.clone());
        let before = std::fs::read_to_string(&config_file).unwrap();
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "insetup",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // Renaming an existing endpoint onto the reserved name is refused
        // too.
        let body = serde_json::json!({
            "name": "insetup",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/endpoints/existing")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(before, after, "rejected mutations must not touch disk");

        // Once the session is gone the name is usable again.
        setup_mgr.remove_session(&session_id).await;
        let body = serde_json::json!({
            "name": "insetup",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .oneshot(
                Request::post("/api/endpoints")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    /// `write_endpoint_to_disk` revalidates name uniqueness against the
    /// on-disk entries, so a create racing a stale in-memory snapshot gets
    /// 409 instead of appending a duplicate `[[endpoints]]` entry.
    #[tokio::test]
    async fn write_endpoint_to_disk_rejects_on_disk_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let _state = endpoints_test_state(&config_file).await;
        let before = std::fs::read_to_string(&config_file).unwrap();

        let dup = EndpointConfig {
            name: "existing".to_string(),
            description: None,
            tool_prefix: None,
            transport: Transport::Stdio,
            command: Some("echo".into()),
            args: None,
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
            isolation: None,
            container_image: None,
            mounts: None,
            auth: None,
        };
        let err = write_endpoint_to_disk(&config_file, &dup, None)
            .expect_err("duplicate create must be rejected");
        assert_eq!(err.0, StatusCode::CONFLICT);

        // Rename onto an existing name is rejected the same way.
        let mut renamed = dup.clone();
        renamed.name = "existing".to_string();
        std::fs::write(
            &config_file,
            format!("{before}\n[[endpoints]]\nname = \"other\"\ntransport = \"stdio\"\ncommand = \"echo\"\n"),
        )
        .unwrap();
        let err = write_endpoint_to_disk(&config_file, &renamed, Some("other"))
            .expect_err("rename onto taken name must be rejected");
        assert_eq!(err.0, StatusCode::CONFLICT);

        // Same-name update (no rename) still succeeds.
        write_endpoint_to_disk(&config_file, &dup, Some("existing"))
            .expect("same-name update must pass the duplicate guard");
    }

    #[tokio::test]
    async fn endpoint_create_rejects_forbidden_and_missing_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let before = std::fs::read_to_string(&config_file).unwrap();
        let app = management_routes(state);

        let cases: Vec<(&str, serde_json::Value)> = vec![
            (
                "client_secret rejected (use /credentials)",
                serde_json::json!({
                    "name": "rej1",
                    "transport": "stdio",
                    "command": "echo",
                    "client_secret": "shh",
                }),
            ),
            (
                "disabled rejected (use /disable)",
                serde_json::json!({
                    "name": "rej2",
                    "transport": "stdio",
                    "command": "echo",
                    "disabled": true,
                }),
            ),
            (
                "disabled_tools rejected (use per-tool route)",
                serde_json::json!({
                    "name": "rej3",
                    "transport": "stdio",
                    "command": "echo",
                    "disabled_tools": ["foo"],
                }),
            ),
            (
                "stdio without command",
                serde_json::json!({ "name": "rej4", "transport": "stdio" }),
            ),
            (
                "http without url",
                serde_json::json!({ "name": "rej5", "transport": "http" }),
            ),
            (
                "sse without url",
                serde_json::json!({ "name": "rej6", "transport": "sse" }),
            ),
            (
                "empty name",
                serde_json::json!({ "name": "", "transport": "stdio", "command": "echo" }),
            ),
        ];

        for (label, body) in cases {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/endpoints")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let s = resp.status();
            assert!(
                s.is_client_error(),
                "case '{}' should reject with a 4xx, got {}",
                label,
                s
            );
        }
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(
            before, after,
            "rejected creates must not write to config.toml"
        );
    }

    #[tokio::test]
    async fn endpoint_create_with_ema_auth_persists_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let app = management_routes(state.clone());

        let body = serde_json::json!({
            "name": "github-acme",
            "transport": "http",
            "url": "https://api.githubcopilot.com/mcp/",
            "auth": {
                "type": "ema",
                "organization": "Acme Corp",
                "resource": "https://api.githubcopilot.com/mcp/",
            },
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/endpoints")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // The `[endpoints.auth]` sub-table is written to config.toml.
        let on_disk = std::fs::read_to_string(&config_file).unwrap();
        assert!(
            on_disk.contains("[endpoints.auth]") || on_disk.contains("[[endpoints]]"),
            "config.toml should contain the new endpoint: {}",
            on_disk
        );

        // Re-parse from disk: the auth binding survives so the watcher /
        // adapter rebuild path sees it as an EMA endpoint.
        let reparsed = crate::config::load_config(&config_file).unwrap();
        let ep = reparsed
            .endpoints
            .iter()
            .find(|e| e.name == "github-acme")
            .expect("created EMA endpoint should be present on disk");
        let auth = ep.auth.as_ref().expect("auth binding must round-trip");
        assert_eq!(auth.auth_type, "ema");
        assert_eq!(auth.organization.as_deref(), Some("Acme Corp"));
        assert_eq!(
            auth.resource.as_deref(),
            Some("https://api.githubcopilot.com/mcp/")
        );

        // In-memory config (post-rebuild) also reflects the auth binding.
        let cfg = state.config.read().await;
        let mem_ep = cfg
            .endpoints
            .iter()
            .find(|e| e.name == "github-acme")
            .unwrap();
        assert_eq!(
            mem_ep.auth.as_ref().map(|a| a.auth_type.as_str()),
            Some("ema")
        );
    }

    #[tokio::test]
    async fn endpoint_create_rejects_invalid_ema_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let before = std::fs::read_to_string(&config_file).unwrap();
        let app = management_routes(state);

        let cases: Vec<(&str, serde_json::Value)> = vec![
            (
                "ema missing resource",
                serde_json::json!({
                    "name": "bad-ema-1",
                    "transport": "http",
                    "url": "https://api.githubcopilot.com/mcp/",
                    "auth": { "type": "ema", "organization": "Acme Corp" },
                }),
            ),
            (
                "ema missing organization and idp",
                serde_json::json!({
                    "name": "bad-ema-2",
                    "transport": "http",
                    "url": "https://api.githubcopilot.com/mcp/",
                    "auth": { "type": "ema", "resource": "https://api.githubcopilot.com/mcp/" },
                }),
            ),
            (
                "unknown auth type",
                serde_json::json!({
                    "name": "bad-ema-3",
                    "transport": "http",
                    "url": "https://api.githubcopilot.com/mcp/",
                    "auth": { "type": "saml", "resource": "https://api.githubcopilot.com/mcp/" },
                }),
            ),
        ];

        for (label, body) in cases {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/api/endpoints")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                resp.status().is_client_error(),
                "case '{}' should reject with a 4xx, got {}",
                label,
                resp.status()
            );
        }
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(
            before, after,
            "rejected EMA creates must not write to config.toml"
        );
    }

    #[tokio::test]
    async fn endpoint_create_without_auth_omits_auth_table() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "plainstdio",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .oneshot(
                Request::post("/api/endpoints")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let reparsed = crate::config::load_config(&config_file).unwrap();
        let ep = reparsed
            .endpoints
            .iter()
            .find(|e| e.name == "plainstdio")
            .unwrap();
        assert!(
            ep.auth.is_none(),
            "no-auth create must not persist an auth binding"
        );
    }

    #[tokio::test]
    async fn endpoint_update_round_trips_ema_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "existing",
            "transport": "http",
            "url": "https://api.githubcopilot.com/mcp/",
            "auth": {
                "type": "ema",
                "idp": "https://acme.okta.com",
                "resource": "https://api.githubcopilot.com/mcp/",
            },
        });
        let resp = app
            .oneshot(
                Request::put("/api/endpoints/existing")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let reparsed = crate::config::load_config(&config_file).unwrap();
        let ep = reparsed
            .endpoints
            .iter()
            .find(|e| e.name == "existing")
            .unwrap();
        let auth = ep.auth.as_ref().expect("updated auth must round-trip");
        assert_eq!(auth.auth_type, "ema");
        assert_eq!(auth.idp.as_deref(), Some("https://acme.okta.com"));
        assert_eq!(
            auth.resource.as_deref(),
            Some("https://api.githubcopilot.com/mcp/")
        );
    }

    #[tokio::test]
    async fn endpoint_update_happy_path_with_rename_preserves_disabled_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        // Mark the existing endpoint disabled with a disabled tool so we
        // can assert the update path preserves both across rename.
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints[0].disabled = true;
            cfg.endpoints[0].disabled_tools = vec!["bad_tool".to_string()];
            let toml_str = toml::to_string_pretty(&*cfg).unwrap();
            std::fs::write(&config_file, toml_str).unwrap();
        }
        let app = management_routes(state.clone());

        let body = serde_json::json!({
            "name": "renamed",
            "transport": "stdio",
            "command": "echo",
            "args": ["after-rename"],
            "description": "updated",
        });
        let resp = app
            .oneshot(
                Request::put("/api/endpoints/existing")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let summary = body_json(resp).await;
        assert_eq!(summary["name"], "renamed");
        assert_eq!(summary["args"][0], "after-rename");

        let on_disk = std::fs::read_to_string(&config_file).unwrap();
        assert!(on_disk.contains("name = \"renamed\""));
        assert!(!on_disk.contains("name = \"existing\""));
        assert!(
            on_disk.contains("disabled = true"),
            "update must preserve disabled=true: {}",
            on_disk
        );
        assert!(
            on_disk.contains("bad_tool"),
            "update must preserve disabled_tools: {}",
            on_disk
        );

        let cfg = state.config.read().await;
        assert!(cfg.endpoints.iter().any(|e| e.name == "renamed"));
        assert!(!cfg.endpoints.iter().any(|e| e.name == "existing"));
    }

    #[tokio::test]
    async fn endpoint_update_unknown_name_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "nope",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .oneshot(
                Request::put("/api/endpoints/nope")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn endpoint_update_rename_conflicts_with_existing_other_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("config.toml");
        let state = endpoints_test_state(&config_file).await;
        // Add a second endpoint we can collide with on rename.
        {
            let mut cfg = state.config.write().await;
            cfg.endpoints.push(EndpointConfig {
                name: "other".to_string(),
                description: None,
                tool_prefix: None,
                transport: Transport::Stdio,
                command: Some("echo".into()),
                args: None,
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
                isolation: None,
                container_image: None,
                mounts: None,
                auth: None,
            });
            let toml_str = toml::to_string_pretty(&*cfg).unwrap();
            std::fs::write(&config_file, toml_str).unwrap();
        }
        let before = std::fs::read_to_string(&config_file).unwrap();
        let app = management_routes(state);

        let body = serde_json::json!({
            "name": "other",
            "transport": "stdio",
            "command": "echo",
        });
        let resp = app
            .oneshot(
                Request::put("/api/endpoints/existing")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let after = std::fs::read_to_string(&config_file).unwrap();
        assert_eq!(
            before, after,
            "rejected rename must not write to config.toml"
        );
    }

    // -------------------------------------------------------------------
    // GET /api/events/tool-calls — desktop overlay SSE stream
    // -------------------------------------------------------------------

    /// 503 when the bus isn't wired (legacy test fixtures / first-run race
    /// between the management socket binding and bus construction).
    #[tokio::test]
    async fn tool_call_events_sse_returns_503_without_bus() {
        let mut state = test_state(vec![]).await;
        state.event_bus = None;
        let router = management_routes(state);
        let req = Request::builder()
            .uri("/api/events/tool-calls")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Bus-wired stream returns 200 with the SSE content-type and an active
    /// keep-alive (so the desktop's Unix-socket HTTP client doesn't drop the
    /// idle connection). We don't assert on the keep-alive comment frame
    /// timing here — the keep-alive interval is 15 s — but we do verify
    /// that a published event reaches the body within a short timeout, and
    /// that subsequent reads keep returning bytes (i.e. the stream is not
    /// closed by axum's keep-alive layer).
    #[tokio::test]
    async fn tool_call_events_sse_streams_published_events() {
        use http_body_util::BodyExt;
        let bus = ToolCallEventBus::with_default_capacity();
        let mut state = test_state(vec![]).await;
        state.event_bus = Some(bus.clone());
        let router = management_routes(state);
        let req = Request::builder()
            .uri("/api/events/tool-calls")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with("text/event-stream"),
            "expected SSE content-type, got {:?}",
            ct
        );

        // Wait for the spawned task to attach its receiver, then publish.
        let mut body = resp.into_body();
        for _ in 0..50 {
            if bus.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(bus.receiver_count() > 0, "SSE handler should subscribe");
        bus.send(crate::events::ToolCallEvent::Completed {
            request_id: "rid-1".into(),
            ts: "2026-05-27T00:00:00.000Z".into(),
            duration_ms: 5,
            status: "ok".into(),
        });

        // Read a few chunks until we observe the JSON payload.
        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let frame = tokio::time::timeout(std::time::Duration::from_millis(200), body.frame())
                .await
                .ok()
                .and_then(|f| f);
            if let Some(Ok(frame)) = frame {
                if let Some(data) = frame.data_ref() {
                    buf.extend_from_slice(data);
                    if String::from_utf8_lossy(&buf).contains("\"kind\":\"completed\"") {
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.contains("\"kind\":\"completed\""),
            "expected 'completed' frame in SSE body, got: {}",
            text
        );
        assert!(
            text.contains("\"request_id\":\"rid-1\""),
            "expected request_id in SSE body, got: {}",
            text
        );
    }

    // -----------------------------------------------------------------------
    // END-19 Wave 3: provider templates + organization lifecycle
    // -----------------------------------------------------------------------

    /// Build a ManagementState wired for organization CRUD: a real config.toml
    /// on disk, a token manager (credential pool), and an OAuth flow manager.
    async fn test_state_orgs(tmp: &std::path::Path, allow_insecure: bool) -> ManagementState {
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, "[relay]\nmachine_name = \"test-machine\"\n").unwrap();
        let token_manager = Arc::new(TokenManager::new(tmp.to_path_buf()));
        let flow_mgr = Arc::new(OAuthFlowManager::new());
        let mut cfg = test_config();
        cfg.relay.allow_insecure_oauth = Some(allow_insecure);
        cfg.organizations = Vec::new();
        ManagementState {
            registry: Arc::new(AdapterRegistry::new()),
            config: Arc::new(RwLock::new(cfg)),
            start_time: Instant::now(),
            config_path: Some(config_path),
            oauth_flow_manager: Some(flow_mgr),
            relay_port: 9400,
            oauth_adapter_inners: None,
            token_manager: Some(token_manager),
            setup_manager: None,
            profile_registry: None,
            event_bus: None,
        }
    }

    /// Mock AS serving RFC 8414 metadata whose issuer matches its own origin.
    /// Advertises CIMD so an org created without an explicit `client_id` resolves
    /// to the hosted CIMD `client_id` (the zero-config public-client default).
    fn org_well_known_router() -> Router {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "code_challenge_methods_supported": ["S256"],
                "client_id_metadata_document_supported": true,
            }))
        }
        Router::new().route("/.well-known/oauth-authorization-server", get(well_known))
    }

    /// Mock AS advertising a `registration_endpoint` (DCR) but NOT CIMD, plus a
    /// `/register` handler returning a fixed dynamically-registered `client_id`.
    fn org_well_known_router_with_dcr() -> Router {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register() -> Json<Value> {
            Json(serde_json::json!({ "client_id": "dcr-registered-client" }))
        }
        Router::new()
            .route("/.well-known/oauth-authorization-server", get(well_known))
            .route("/register", axum::routing::post(register))
    }

    /// Mock AS advertising NEITHER CIMD nor DCR: an org created here without an
    /// explicit `client_id` cannot resolve one and must return `422`.
    fn org_well_known_router_no_client() -> Router {
        async fn well_known(headers: axum::http::HeaderMap) -> Json<Value> {
            let issuer = mock_issuer(&headers);
            Json(serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        Router::new().route("/.well-known/oauth-authorization-server", get(well_known))
    }

    #[tokio::test]
    async fn idp_providers_endpoint_returns_table() {
        let state = test_state(vec![]).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::get("/api/idp-providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().expect("providers array");
        let ids: Vec<&str> = arr.iter().filter_map(|p| p["id"].as_str()).collect();
        for expected in ["okta", "entra", "google", "ping", "custom"] {
            assert!(
                ids.contains(&expected),
                "missing provider '{expected}' in {ids:?}"
            );
        }
        // Templated providers carry an issuer_pattern; custom does not.
        let okta = arr.iter().find(|p| p["id"] == "okta").unwrap();
        assert_eq!(okta["issuer_pattern"], "https://{slug}.okta.com");
        let custom = arr.iter().find(|p| p["id"] == "custom").unwrap();
        assert!(custom.get("issuer_pattern").is_none());
    }

    #[tokio::test]
    async fn create_organization_custom_validates_and_returns_sso_url() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        let config_path = state.config_path.clone().unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "Acme Corp");
        assert_eq!(body["idp"], base_url);
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.starts_with(&format!("{base_url}/authorize?")),
            "expected discovered authorize endpoint, got: {authorize_url}"
        );
        assert!(authorize_url.contains("response_type=code"));
        assert!(authorize_url.contains("scope=openid"));

        // Persisted both in memory and on disk.
        assert_eq!(config.read().await.organizations.len(), 1);
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(disk.contains("[[organizations]]"));
        assert!(disk.contains("Acme Corp"));
        // Tokens are never written to config.toml.
        assert!(!disk.contains("id_token"));
    }

    #[tokio::test]
    async fn create_organization_rejects_bad_issuer_pre_save() {
        async fn not_found() -> StatusCode {
            StatusCode::NOT_FOUND
        }
        let router = Router::new().route("/.well-known/oauth-authorization-server", get(not_found));
        let (base_url, _server) = spawn_mock_as(router).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Bad Org",
                            "provider": "custom",
                            "idp": base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_issuer");
        // Bad issuer must NOT be persisted.
        assert!(config.read().await.organizations.is_empty());
    }

    #[tokio::test]
    async fn create_organization_blocks_loopback_without_allow_insecure() {
        // allow_insecure=false → the discovery SSRF guard rejects the loopback
        // mock host before any metadata is fetched, so the org is not saved.
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), false).await;
        let config = state.config.clone();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "SSRF Org",
                            "provider": "custom",
                            "idp": base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_issuer");
        assert!(config.read().await.organizations.is_empty());
    }

    #[tokio::test]
    async fn create_organization_rejects_unknown_provider_and_missing_idp() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let app = management_routes(state);

        // Unknown provider id.
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "name": "X", "provider": "nope" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "unknown_provider");

        // Custom without an idp URL.
        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "name": "Y", "provider": "custom" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "missing_idp");
    }

    #[tokio::test]
    async fn organization_lifecycle_get_delete_purges_credential_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        let config = state.config.clone();

        // Seed an org + a credential-pool entry keyed by the org name.
        config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "okta".to_string(),
            idp: "https://acme.okta.com".to_string(),
            client_id: None,
        }];
        tm.save_idp(
            "Acme Corp",
            &crate::token_manager::IdpCredentials {
                idp_issuer: "https://acme.okta.com".to_string(),
                id_token: "id-tok".to_string(),
                refresh_token: Some("refresh-tok".to_string()),
                id_token_expires_at: None,
                obtained_at: 0,
            },
        )
        .await
        .unwrap();

        let app = management_routes(state);

        // GET reports the org as authenticated (creds present).
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/organizations")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "Acme Corp");
        assert_eq!(arr[0]["authenticated"], true);

        // DELETE removes the org and purges its credentials.
        let resp = app
            .clone()
            .oneshot(
                Request::delete("/api/organizations/Acme%20Corp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(config.read().await.organizations.is_empty());
        assert!(tm.load_idp("Acme Corp").await.unwrap().is_none());

        // GET now returns an empty list.
        let resp = app
            .oneshot(
                Request::get("/api/organizations")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_json(resp).await;
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_organization_missing_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::delete("/api/organizations/ghost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reauthenticate_organization_returns_fresh_sso_url() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: None,
        }];
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations/Acme%20Corp/reauthenticate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "Acme Corp");
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(authorize_url.starts_with(&format!("{base_url}/authorize?")));
        assert!(authorize_url.contains("scope=openid"));
    }

    /// Resolution chain — explicit: an org created with an explicit `client_id`
    /// uses it verbatim in the authorize URL (over the CIMD the AS advertises)
    /// and persists it on the org both in memory and on disk.
    #[tokio::test]
    async fn create_organization_explicit_client_id_wins_and_is_persisted() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        let config_path = state.config_path.clone().unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                            "client_id": "explicit-okta-client",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.contains("client_id=explicit-okta-client"),
            "explicit client_id must win over CIMD, got: {authorize_url}"
        );

        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].client_id.as_deref(), Some("explicit-okta-client"));
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(disk.contains("explicit-okta-client"));
    }

    /// Resolution chain — CIMD: an org on a CIMD-advertising AS with no explicit
    /// `client_id` uses the hosted CIMD `client_id` and persists `None` (so the
    /// org config round-trips unchanged).
    #[tokio::test]
    async fn create_organization_cimd_used_when_no_explicit_client_id() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        let config_path = state.config_path.clone().unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.contains(&format!(
                "client_id={}",
                urlencoding(crate::oauth::client::ENDARA_CLIENT_METADATA_URL)
            )),
            "CIMD client_id expected, got: {authorize_url}"
        );

        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs.len(), 1);
        assert!(orgs[0].client_id.is_none(), "CIMD must persist None");
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(!disk.contains("client_id"));
    }

    /// Resolution chain — DCR: an org on an AS that advertises DCR (no CIMD, no
    /// explicit client_id) registers a client and uses/persists the registered
    /// `client_id`.
    #[tokio::test]
    async fn create_organization_dcr_registers_and_persists_client_id() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router_with_dcr()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();
        assert!(
            authorize_url.contains("client_id=dcr-registered-client"),
            "DCR-registered client_id expected, got: {authorize_url}"
        );

        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].client_id.as_deref(), Some("dcr-registered-client"));
    }

    /// Resolution chain — 422: an org on an AS advertising neither CIMD nor DCR,
    /// with no explicit `client_id`, returns `422 client_id_required` and is not
    /// persisted.
    #[tokio::test]
    async fn create_organization_returns_422_when_no_client_id_resolvable() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router_no_client()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "client_id_required");
        assert!(
            config.read().await.organizations.is_empty(),
            "org must not be persisted when client_id is unresolvable"
        );
    }

    /// Confidential client: an org created with `client_secret` persists the
    /// secret to the secure credential store (`{org}.dcr.json`), NEVER to
    /// `config.toml`, and the composed SSO flow carries the secret so the
    /// auth-code exchange in `/oauth/callback` authenticates with it.
    #[tokio::test]
    async fn create_organization_with_client_secret_persists_dcr_and_threads_flow_secret() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let flow_mgr = state.oauth_flow_manager.clone().unwrap();
        let token_manager = state.token_manager.clone().unwrap();
        let config = state.config.clone();
        let config_path = state.config_path.clone().unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                            "client_id": "explicit-okta-client",
                            "client_secret": "super-secret-value",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();

        // The DCR file must hold the resolved client_id + the secret.
        let loaded = token_manager
            .load_dcr("Acme Corp")
            .await
            .unwrap()
            .expect("DCR credentials should be persisted for the org");
        assert_eq!(loaded.client_id, "explicit-okta-client");
        assert_eq!(loaded.client_secret.as_deref(), Some("super-secret-value"));
        assert_eq!(loaded.issuer.as_deref(), Some(base_url.as_str()));

        // config.toml must NOT contain the secret (only public org fields).
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !disk.contains("super-secret-value"),
            "client_secret must never be written to config.toml; got: {disk}"
        );
        assert!(!disk.contains("client_secret"));
        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].client_id.as_deref(), Some("explicit-okta-client"));

        // The pending flow registered with the OAuthFlowManager must carry the
        // secret so /oauth/callback includes it in the form body.
        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending IdP flow was registered");
        assert_eq!(flow.client_id, "explicit-okta-client");
        assert_eq!(
            flow.client_secret.as_deref(),
            Some("super-secret-value"),
            "create_organization must thread client_secret into the pending flow"
        );
    }

    /// Public/PKCE org: omitting `client_secret` MUST keep the existing
    /// behaviour byte-for-byte — no DCR file is written and the pending flow
    /// carries no secret.
    #[tokio::test]
    async fn create_organization_without_client_secret_keeps_public_pkce_behaviour() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let flow_mgr = state.oauth_flow_manager.clone().unwrap();
        let token_manager = state.token_manager.clone().unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::post("/api/organizations")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Acme Corp",
                            "provider": "custom",
                            "idp": base_url,
                            "client_id": "explicit-okta-client",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();

        assert!(
            token_manager.load_dcr("Acme Corp").await.unwrap().is_none(),
            "no DCR file should be written when client_secret is omitted"
        );

        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending IdP flow was registered");
        assert!(
            flow.client_secret.is_none(),
            "public/PKCE flow must not carry a client_secret; got {:?}",
            flow.client_secret
        );
    }

    /// Re-authenticate threads the previously-stored client_secret into the
    /// pending flow so the second authorize → token exchange uses it just like
    /// the first one.
    #[tokio::test]
    async fn reauthenticate_organization_threads_stored_client_secret() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let flow_mgr = state.oauth_flow_manager.clone().unwrap();
        let token_manager = state.token_manager.clone().unwrap();

        // Seed the org config + DCR store as if creation had captured a secret.
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: Some("explicit-okta-client".to_string()),
        }];
        token_manager
            .save_dcr(
                "Acme Corp",
                &DcrCredentials {
                    client_id: "explicit-okta-client".to_string(),
                    client_secret: Some("super-secret-value".to_string()),
                    client_secret_expires_at: 0,
                    registered_at: 0,
                    issuer: Some(base_url.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::post("/api/organizations/Acme%20Corp/reauthenticate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"].as_str().unwrap();

        let state_param = extract_state_param(authorize_url);
        let flow = flow_mgr
            .consume_flow(&state_param)
            .await
            .expect("pending IdP flow was registered on reauth");
        assert_eq!(flow.client_id, "explicit-okta-client");
        assert_eq!(
            flow.client_secret.as_deref(),
            Some("super-secret-value"),
            "reauthenticate must load and thread the stored client_secret"
        );
    }

    /// PUT against an unknown org returns 404 and leaves config untouched.
    #[tokio::test]
    async fn update_organization_missing_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let app = management_routes(state);
        let resp = app
            .oneshot(
                Request::put("/api/organizations/ghost")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({"name": "ghost"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Field-level update with no identity change (provider/idp/client_id all
    /// match the existing org) returns the refreshed org metadata WITHOUT an
    /// authorize_url and preserves pooled IdP credentials.
    #[tokio::test]
    async fn update_organization_no_identity_change_preserves_creds_and_returns_org() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: None,
        }];
        tm.save_idp(
            "Acme Corp",
            &crate::token_manager::IdpCredentials {
                idp_issuer: base_url.clone(),
                id_token: "id-tok".to_string(),
                refresh_token: Some("refresh-tok".to_string()),
                id_token_expires_at: None,
                obtained_at: 0,
            },
        )
        .await
        .unwrap();
        let app = management_routes(state);

        // Send a PUT with an empty body — everything preserved.
        let resp = app
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body.get("authorize_url").is_none(),
            "no authorize_url should be returned when identity is unchanged; got: {body}"
        );
        assert_eq!(body["name"], "Acme Corp");
        assert_eq!(body["authenticated"], true);
        // IdP credentials must still be present.
        assert!(tm.load_idp("Acme Corp").await.unwrap().is_some());
    }

    /// Changing the issuer (provider/slug/idp) purges pooled IdP credentials
    /// so the org flips back to "Sign-in required", and the response carries
    /// a fresh authorize URL pointing at the NEW issuer.
    #[tokio::test]
    async fn update_organization_issuer_change_purges_pooled_credentials() {
        let (old_base_url, _old_server) = spawn_mock_as(org_well_known_router()).await;
        let (new_base_url, _new_server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        let config = state.config.clone();
        let config_path = state.config_path.clone().unwrap();
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: old_base_url.clone(),
            client_id: None,
        }];
        tm.save_idp(
            "Acme Corp",
            &crate::token_manager::IdpCredentials {
                idp_issuer: old_base_url.clone(),
                id_token: "id-tok".to_string(),
                refresh_token: Some("refresh-tok".to_string()),
                id_token_expires_at: None,
                obtained_at: 0,
            },
        )
        .await
        .unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "provider": "custom",
                            "idp": new_base_url,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["idp"], new_base_url);
        let authorize_url = body["authorize_url"]
            .as_str()
            .expect("issuer change must return an authorize_url since pooled creds were purged");
        assert!(
            authorize_url.starts_with(&format!("{new_base_url}/authorize?")),
            "authorize_url should target the NEW issuer, got: {authorize_url}"
        );

        // Pooled IdP credentials must be gone — status flips to "Sign-in required".
        assert!(tm.load_idp("Acme Corp").await.unwrap().is_none());
        // config.toml + in-memory both reflect the new issuer.
        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs[0].idp, new_base_url);
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(disk.contains(&new_base_url));
        assert!(!disk.contains(&old_base_url));
    }

    /// Changing the explicit `client_id` purges pooled IdP credentials and
    /// the stale DCR record, then persists the new id.
    #[tokio::test]
    async fn update_organization_client_id_change_purges_pooled_credentials() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        let config = state.config.clone();
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: Some("old-client".to_string()),
        }];
        tm.save_idp(
            "Acme Corp",
            &crate::token_manager::IdpCredentials {
                idp_issuer: base_url.clone(),
                id_token: "id-tok".to_string(),
                refresh_token: Some("refresh-tok".to_string()),
                id_token_expires_at: None,
                obtained_at: 0,
            },
        )
        .await
        .unwrap();
        tm.save_dcr(
            "Acme Corp",
            &DcrCredentials {
                client_id: "old-client".to_string(),
                client_secret: Some("old-secret".to_string()),
                client_secret_expires_at: 0,
                registered_at: 0,
                issuer: Some(base_url.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"client_id": "new-client"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let authorize_url = body["authorize_url"]
            .as_str()
            .expect("client_id change must return an authorize_url");
        assert!(
            authorize_url.contains("client_id=new-client"),
            "authorize_url should carry the new client_id, got: {authorize_url}"
        );

        assert!(tm.load_idp("Acme Corp").await.unwrap().is_none());
        // The stale DCR (bound to the old client_id) must be purged.
        assert!(tm.load_dcr("Acme Corp").await.unwrap().is_none());
        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs[0].client_id.as_deref(), Some("new-client"));
    }

    /// `client_secret` lifecycle: set on a public/PKCE org, replace it, then
    /// clear it. The DCR file is created, overwritten, then deleted; the
    /// config.toml is never touched.
    #[tokio::test]
    async fn update_organization_client_secret_set_replace_clear() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        let config_path = state.config_path.clone().unwrap();
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: Some("explicit-okta-client".to_string()),
        }];
        let app = management_routes(state);

        // 1. SET — supply a secret on an org that had none.
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"client_secret": "secret-v1"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm
            .load_dcr("Acme Corp")
            .await
            .unwrap()
            .expect("DCR file should exist after secret set");
        assert_eq!(creds.client_id, "explicit-okta-client");
        assert_eq!(creds.client_secret.as_deref(), Some("secret-v1"));
        assert_eq!(creds.issuer.as_deref(), Some(base_url.as_str()));
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(!disk.contains("secret-v1"));
        assert!(!disk.contains("client_secret"));

        // 2. REPLACE — overwrite with a new secret.
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"client_secret": "secret-v2"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm
            .load_dcr("Acme Corp")
            .await
            .unwrap()
            .expect("DCR file still present after replace");
        assert_eq!(creds.client_secret.as_deref(), Some("secret-v2"));
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(!disk.contains("secret-v1"));
        assert!(!disk.contains("secret-v2"));

        // 3. CLEAR — empty string deletes the DCR record.
        let resp = app
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"client_secret": ""}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tm.load_dcr("Acme Corp").await.unwrap().is_none(),
            "DCR file should be removed when client_secret is explicitly cleared"
        );
    }

    /// R3: the org route still captures the requesting `client_secret`, but the
    /// EMA **resource** credential pair has moved to the endpoint (R3/D2) — any
    /// resource fields posted to `/api/organizations` are now ignored and never
    /// land in `{org}.dcr.json`.
    #[tokio::test]
    async fn update_organization_ignores_resource_creds_keeps_requesting_secret() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        let config_path = state.config_path.clone().unwrap();
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: Some("explicit-okta-client".to_string()),
        }];
        let app = management_routes(state);

        // Supply the requesting secret AND (now-ignored) resource fields.
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "client_secret": "req-secret",
                            "resource_client_id": "res-client",
                            "resource_client_secret": "res-secret",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let creds = tm
            .load_dcr("Acme Corp")
            .await
            .unwrap()
            .expect("DCR file should exist after set");
        assert_eq!(creds.client_secret.as_deref(), Some("req-secret"));
        assert!(
            creds.resource_client_id.is_none(),
            "org route must not persist a resource_client_id (moved to endpoint)"
        );
        assert!(
            creds.resource_client_secret.is_none(),
            "org route must not persist a resource_client_secret (moved to endpoint)"
        );
        // Secrets never leak into config.toml.
        let disk = std::fs::read_to_string(&config_path).unwrap();
        assert!(!disk.contains("req-secret"));
        assert!(!disk.contains("res-secret"));
    }

    /// Rename purges pooled credentials at both keys and removes the old DCR,
    /// returning an authorize_url so the user re-runs SSO under the new name.
    #[tokio::test]
    async fn update_organization_rename_purges_credentials_at_both_keys() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let tm = state.token_manager.clone().unwrap();
        let config = state.config.clone();
        state.config.write().await.organizations = vec![crate::config::ConfigOrganization {
            name: "Acme Corp".to_string(),
            provider: "custom".to_string(),
            idp: base_url.clone(),
            client_id: None,
        }];
        tm.save_idp(
            "Acme Corp",
            &crate::token_manager::IdpCredentials {
                idp_issuer: base_url.clone(),
                id_token: "id-tok".to_string(),
                refresh_token: Some("refresh-tok".to_string()),
                id_token_expires_at: None,
                obtained_at: 0,
            },
        )
        .await
        .unwrap();
        tm.save_dcr(
            "Acme Corp",
            &DcrCredentials {
                client_id: "old-client".to_string(),
                client_secret: Some("old-secret".to_string()),
                client_secret_expires_at: 0,
                registered_at: 0,
                issuer: Some(base_url.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"name": "Acme Inc"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["name"], "Acme Inc");
        assert!(
            body.get("authorize_url").is_some(),
            "rename must return a fresh authorize_url; got: {body}"
        );

        // Both the old and new IdP keys must be empty; the old DCR must be gone.
        assert!(tm.load_idp("Acme Corp").await.unwrap().is_none());
        assert!(tm.load_idp("Acme Inc").await.unwrap().is_none());
        assert!(tm.load_dcr("Acme Corp").await.unwrap().is_none());

        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].name, "Acme Inc");
    }

    /// Rename onto a name that already exists returns 409 and leaves the org
    /// list untouched.
    #[tokio::test]
    async fn update_organization_rename_conflict_returns_409() {
        let (base_url, _server) = spawn_mock_as(org_well_known_router()).await;
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state_orgs(tmp.path(), true).await;
        let config = state.config.clone();
        state.config.write().await.organizations = vec![
            crate::config::ConfigOrganization {
                name: "Acme Corp".to_string(),
                provider: "custom".to_string(),
                idp: base_url.clone(),
                client_id: None,
            },
            crate::config::ConfigOrganization {
                name: "Acme Inc".to_string(),
                provider: "custom".to_string(),
                idp: base_url.clone(),
                client_id: None,
            },
        ];
        let app = management_routes(state);

        let resp = app
            .oneshot(
                Request::put("/api/organizations/Acme%20Corp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"name": "Acme Inc"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let orgs = config.read().await.organizations.clone();
        assert_eq!(orgs.len(), 2);
        assert!(orgs.iter().any(|o| o.name == "Acme Corp"));
    }
}
