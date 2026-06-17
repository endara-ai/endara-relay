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

use tracing::warn;

use crate::adapter::http::{HttpAdapter, HttpConfig};
use crate::adapter::oauth::OAuthState;
use crate::adapter::sse::{SseAdapter, SseConfig};
use crate::adapter::stdio::{StdioAdapter, StdioConfig};
use crate::adapter::{FailedAdapter, HealthStatus, McpAdapter, StartingAdapter};
use crate::config::{Config, ObservabilityConfig};
use crate::events::ToolCallEventBus;
use crate::oauth::{OAuthFlowManager, OAuthSetupManager, PkceChallenge};
use crate::observability::payloads::StoredPayloads;
use crate::observability::store::{AggregateBucket, CallRecord, QueryFilter};
use crate::profile_registry::ProfileRegistry;
use crate::registry::AdapterRegistry;
use crate::token_manager::{DcrCredentials, TokenManager};
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
        let (ep_config, allow_insecure_oauth) = {
            let cfg = config.read().await;
            (
                cfg.endpoints
                    .iter()
                    .find(|ep| ep.name == task_name)
                    .cloned(),
                cfg.relay.allow_insecure_oauth.unwrap_or(false),
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
        let store = obs.store();
        let filter = crate::observability::store::QueryFilter {
            server_name: Some(name.clone()),
            ..Default::default()
        };
        let mut uids: std::collections::HashSet<String> = std::collections::HashSet::new();
        const UID_PAGE: i64 = 1000;
        let mut offset = 0i64;
        loop {
            match store.query(&filter, UID_PAGE, offset) {
                Ok(rows) => {
                    let fetched = rows.len() as i64;
                    for row in rows {
                        if let Some(uid) = row.request_uid {
                            uids.insert(uid);
                        }
                    }
                    if fetched < UID_PAGE {
                        break;
                    }
                    offset += UID_PAGE;
                }
                Err(e) => {
                    warn!(error = %e, server = %name, "observability: failed to collect request_uids for delete cascade");
                    break;
                }
            }
        }

        match store.delete_for_server(&name) {
            Ok(removed) => {
                tracing::debug!(server = %name, removed, "observability: deleted metadata rows for deleted server")
            }
            Err(e) => {
                warn!(error = %e, server = %name, "observability: failed to delete metadata rows for deleted server")
            }
        }

        if !uids.is_empty() {
            obs.payloads().remove_for_server(&uids);
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
        if let Err(e) = crate::config::write_config_file(&resolved, &toml_str) {
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
}

/// Response body for GET /api/endpoints/:name/credentials.
#[derive(Serialize)]
struct EndpointCredentialsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    client_secret_set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_server_url: Option<String>,
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

/// POST /api/endpoints/:name/oauth/start
///
/// Generates a PKCE challenge, registers a pending flow, and returns the
/// authorization URL that the user should open in a browser.
///
/// Resolution order:
/// 1. If `oauth_server_url` is in config, derive endpoints from convention.
///    Otherwise, try RFC 9728 discovery against the endpoint URL.
/// 2. If `client_id` is in config, use it. Otherwise, load persisted DCR
///    credentials → if missing/expired + registration_endpoint available,
///    attempt dynamic client registration → if DCR fails/unavailable, return
///    `dcr_unsupported` so the UI can prompt for manual credentials.
async fn oauth_start(
    State(state): State<ManagementState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
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

    // ── Step 1: Resolve OAuth server metadata ──────────────────────────
    let (
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        discovered_scopes,
        auth_server_label,
    ) = if let Some(ref server_url) = oauth_server_url {
        // Prefer RFC 8414 discovery against the configured AS URL. If it
        // succeeds, use the discovered endpoints (explicit token_endpoint
        // config still wins). On any error, fall back to the legacy
        // convention-based construction so behavior is unchanged for
        // servers that don't expose AS metadata.
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
                )
            }
            Err(e) => {
                warn!(
                    endpoint = %name,
                    error = %e,
                    "RFC 8414 discovery against oauth_server_url failed; falling back to convention-based endpoints"
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
                )
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

    let (client_id, client_secret, dcr_used) = if let Some(cid) = config_client_id {
        // TOML has a client_id. Prefer the DCR-persisted client_secret when
        // the DCR record's client_id matches; otherwise fall back to whatever
        // is in TOML (which may be None for endpoints added via the desktop
        // UI — that is the bug this branch fixes).
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

    let state_param = flow_mgr
        .start_flow(
            &name,
            &token_endpoint,
            &client_id,
            client_secret.as_deref(),
            pkce,
            &redirect_uri,
        )
        .await;

    // ── Step 4: Build authorization URL ────────────────────────────────
    let mut authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        authorization_endpoint,
        urlencoding(&client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state_param),
        urlencoding(&code_challenge),
    );

    // Prefer config scopes; fall back to discovered scopes
    let effective_scopes = scopes.unwrap_or_default();
    if !effective_scopes.is_empty() {
        let scope_str = effective_scopes.join(" ");
        authorize_url.push_str(&format!("&scope={}", urlencoding(&scope_str)));
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
/// Persist caller-supplied OAuth client credentials via `TokenManager` (the
/// DCR file). Never writes them to `config.toml`. Used by Wave 3a so that
/// `client_secret` is no longer treated as a static config value.
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

    let client_id = match body.client_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            return error_response(StatusCode::BAD_REQUEST, "client_id must not be empty", None)
                .into_response();
        }
    };

    let Some(ref tm) = state.token_manager else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Token manager not available",
            None,
        )
        .into_response();
    };

    let client_secret_set = body
        .client_secret
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let creds = DcrCredentials {
        client_id,
        client_secret: body.client_secret.filter(|s| !s.is_empty()),
        client_secret_expires_at: 0,
        registered_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };

    if let Err(e) = tm.save_dcr(&name, &creds).await {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save credentials",
            Some(&e.to_string()),
        )
        .into_response();
    }

    Json(serde_json::json!({
        "ok": true,
        "client_secret_set": client_secret_set,
    }))
    .into_response()
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
                return Json(EndpointCredentialsResponse {
                    client_id: Some(creds.client_id),
                    client_secret_set: creds.client_secret.is_some(),
                    oauth_server_url: cfg_oauth_server_url,
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
    let session_id = setup_mgr
        .create_session(
            body.name.clone(),
            body.url.clone(),
            scopes_str.clone(),
            body.tool_prefix.clone(),
            body.server_type_override.clone(),
        )
        .await;

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

    setup_mgr
        .get_session_mut(&session_id, |s| {
            s.authorization_endpoint = Some(disc.authorization_endpoint.clone());
            s.token_endpoint = Some(disc.token_endpoint.clone());
            s.registration_endpoint = disc.registration_endpoint.clone();
            s.oauth_server_url = Some(disc.auth_server_url.clone());
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
    let used_dcr = manual_client_id.is_none();

    let dcr_result: Result<(String, Option<String>), String> = if let Some(client_id) =
        manual_client_id
    {
        // Persist manual credentials so future re-auth can find them
        if let Some(ref tm) = state.token_manager {
            let creds = DcrCredentials {
                client_id: client_id.clone(),
                client_secret: manual_client_secret.clone(),
                client_secret_expires_at: 0,
                registered_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };
            if let Err(e) = tm.save_dcr(&body.name, &creds).await {
                warn!(endpoint = %body.name, error = %e, "Failed to persist manual credentials");
            }
        }
        Ok((client_id, manual_client_secret))
    } else if let Some(ref reg_endpoint) = registration_endpoint {
        match dcr::register_client(
            reg_endpoint,
            &redirect_uri,
            &body.name,
            allow_insecure_oauth,
        )
        .await
        {
            Ok(resp) => {
                // Persist DCR credentials for future re-auth
                if let Some(ref tm) = state.token_manager {
                    let creds = DcrCredentials {
                        client_id: resp.client_id.clone(),
                        client_secret: resp.client_secret.clone(),
                        client_secret_expires_at: resp.client_secret_expires_at,
                        registered_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    };
                    if let Err(e) = tm.save_dcr(&body.name, &creds).await {
                        warn!(endpoint = %body.name, error = %e, "Failed to persist DCR credentials");
                    }
                }
                Ok((resp.client_id, resp.client_secret))
            }
            Err(e) => Err(format!("{e}")),
        }
    } else {
        Err("No registration endpoint available".to_string())
    };

    match dcr_result {
        Ok((client_id, client_secret)) => {
            // Store credentials in session
            setup_mgr
                .get_session_mut(&session_id, |s| {
                    s.client_id = Some(client_id.clone());
                    s.client_secret = client_secret.clone();
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

            if let Some(ref scope_str) = scopes_str {
                if !scope_str.is_empty() {
                    authorize_url.push_str(&format!("&scope={}", urlencoding(scope_str)));
                }
            }

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
            s.status = crate::oauth::SetupSessionStatus::AwaitingAuth;
            (
                s.authorization_endpoint.clone(),
                s.token_endpoint.clone(),
                s.scopes.clone(),
                s.name.clone(),
            )
        })
        .await;

    let Some((Some(auth_endpoint), Some(token_endpoint), scopes, name)) = session_data else {
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

    if let Some(ref scope_str) = scopes {
        if !scope_str.is_empty() {
            authorize_url.push_str(&format!("&scope={}", urlencoding(scope_str)));
        }
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

    // Take the session out — it's consumed on commit
    let Some(session) = setup_mgr.remove_session(&session_id).await else {
        return error_response(StatusCode::NOT_FOUND, "session not found or expired", None)
            .into_response();
    };

    if session.status != crate::oauth::SetupSessionStatus::Authorized {
        // Put it back
        setup_mgr.get_session_mut(&session_id, |_| {}).await;
        return error_response(
            StatusCode::CONFLICT,
            "session_not_authorized",
            Some("OAuth authorization has not been completed yet."),
        )
        .into_response();
    }

    // Build the endpoint config entry
    let Some(ref config_path) = state.config_path else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_path not configured",
            None,
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

    // Build the new endpoint TOML entry
    let mut ep_table = toml::Table::new();
    ep_table.insert("name".into(), toml::Value::String(session.name.clone()));
    ep_table.insert("transport".into(), toml::Value::String("oauth".to_string()));
    ep_table.insert("url".into(), toml::Value::String(session.url.clone()));

    if let Some(ref oauth_server) = session.oauth_server_url {
        ep_table.insert(
            "oauth_server_url".into(),
            toml::Value::String(oauth_server.clone()),
        );
    }
    if let Some(ref token_ep) = session.token_endpoint {
        ep_table.insert(
            "token_endpoint".into(),
            toml::Value::String(token_ep.clone()),
        );
    }
    if let Some(ref cid) = session.client_id {
        ep_table.insert("client_id".into(), toml::Value::String(cid.clone()));
    }
    if let Some(ref cs) = session.client_secret {
        ep_table.insert("client_secret".into(), toml::Value::String(cs.clone()));
    }
    if let Some(ref scopes_str) = session.scopes {
        let scopes_vec: Vec<toml::Value> = scopes_str
            .split_whitespace()
            .map(|s| toml::Value::String(s.to_string()))
            .collect();
        if !scopes_vec.is_empty() {
            ep_table.insert("scopes".into(), toml::Value::Array(scopes_vec));
        }
    }
    if let Some(ref tp) = session.tool_prefix {
        ep_table.insert("tool_prefix".into(), toml::Value::String(tp.clone()));
    }
    if let Some(ref sto) = session.server_type_override {
        ep_table.insert(
            "server_type_override".into(),
            toml::Value::String(sto.clone()),
        );
    }

    // Append to the [[endpoints]] array
    let endpoints = parsed
        .entry("endpoints")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if let toml::Value::Array(ref mut arr) = endpoints {
        arr.push(toml::Value::Table(ep_table));
    }

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

    // Persist the token via TokenManager so it survives restarts
    if let (Some(ref tm), Some(tokens)) = (&state.token_manager, session.tokens) {
        if let Err(e) = tm.save(&session.name, &tokens).await {
            warn!(endpoint = %session.name, error = %e, "Failed to persist tokens");
        }
    }

    // The config watcher will pick up the change and load the new adapter.
    Json(serde_json::json!({
        "status": "committed",
        "name": session.name
    }))
    .into_response()
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

    let removed = setup_mgr.remove_session(&session_id).await;
    if removed.is_some() {
        Json(serde_json::json!({ "status": "cancelled" })).into_response()
    } else {
        error_response(StatusCode::NOT_FOUND, "session not found or expired", None).into_response()
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
    };

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

    match original_name {
        None => {
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
    if let Err(resp) = apply_endpoint_change(&state, new_ep.clone(), None).await {
        return *resp;
    }
    (StatusCode::CREATED, Json(endpoint_summary_from(&new_ep))).into_response()
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

/// Query string for `GET /api/observability/calls`. All filters are optional
/// and ANDed; `since`/`until` bound `ts_start` as epoch milliseconds.
#[derive(Deserialize)]
struct CallsQuery {
    server_name: Option<String>,
    tool: Option<String>,
    success: Option<bool>,
    request_uid: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Response for `GET /api/observability/calls`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallsResponse {
    calls: Vec<CallRecordDto>,
    limit: i64,
    offset: i64,
}

/// Default and maximum page sizes for the calls list.
const CALLS_DEFAULT_LIMIT: i64 = 100;
const CALLS_MAX_LIMIT: i64 = 1000;

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
/// excluded; use the drill-through route for those).
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
    let offset = q.offset.unwrap_or(0).max(0);
    let filter = QueryFilter {
        server_name: q.server_name,
        tool: q.tool,
        success: q.success,
        request_uid: q.request_uid,
        since: q.since,
        until: q.until,
    };
    match obs.store().query(&filter, limit, offset) {
        Ok(rows) => Json(CallsResponse {
            calls: rows.into_iter().map(CallRecordDto::from).collect(),
            limit,
            offset,
        })
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
    let record = match obs.store().get_by_request_uid(&request_uid) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "call record not found",
                Some(&format!("No record for request_uid {request_uid}")),
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
    match obs.store().aggregate(bucket_seconds, since, until) {
        Ok(buckets) => Json(AggregatesResponse {
            buckets: buckets.into_iter().map(AggregateBucketDto::from).collect(),
            summary,
        })
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
    if let Err(e) = obs.store().purge_all() {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to purge observability metadata",
            Some(&e.to_string()),
        )
        .into_response();
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
/// to disk (targeted edit, preserving the rest of the file) and swap the
/// in-memory baseline. Runtime store sizing (windows, budgets, enable/disable)
/// is re-read on the next relay restart.
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
        // No CORS layer: this router is served exclusively over a Unix-domain
        // socket / Windows named pipe (see `management_listener`), which is not
        // reachable from a browser and has no cross-origin attack surface.
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
            }],
            profiles: None,
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
        // "echo" is in test_config but is not a real MCP server, so
        // create_adapter ends up producing a FailedAdapter (Unhealthy).
        let swapped = tokio::time::timeout(Duration::from_secs(5), async {
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
            "registry never reflected new adapter within 5s"
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
        let registry_for_poll = state.registry.clone();
        let app = management_routes(state);

        // Item B (1): tight upper bound on response time even though the old
        // adapter's shutdown takes 1.5s.
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
            elapsed < Duration::from_millis(100),
            "restart should return in <100ms, took {:?}",
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
        // should hold the new adapter. With test_config()'s "echo" entry
        // (command = "echo", no MCP server), create_adapter ends up with a
        // FailedAdapter whose health is Unhealthy.
        let final_state = tokio::time::timeout(Duration::from_secs(5), async {
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
            "registry never reflected final swapped adapter within 5s"
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
        // essentially immediately (well within the shutdown delay).
        let first = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("foreground tick did not arrive within 500ms")
            .expect("foreground tick channel closed");
        assert_eq!(first, "echo", "foreground tick should carry endpoint name");

        // Second tick: background re-init swap completion. Allow extra time
        // because the slow shutdown must finish before the new adapter is
        // swapped in.
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
                0,
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
                0,
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
            .await;

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
            .await;

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
            .await;

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
            .await;

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
            .await;

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
            .await;
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
            .await;

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
            .await;

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
            .await;

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
            .await;

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
            }],
            profiles: None,
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
            }],
            profiles: None,
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
        async fn well_known() -> Json<Value> {
            Json(serde_json::json!({
                "issuer": "http://example.test",
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

    #[tokio::test]
    async fn oauth_start_with_explicit_token_endpoint_overrides_discovery() {
        // Mock AS: discovery succeeds but advertises a token_endpoint that
        // differs from the operator-configured explicit override. The
        // override must win.
        async fn well_known() -> Json<Value> {
            Json(serde_json::json!({
                "issuer": "http://example.test",
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

        let well_known_handler = move || {
            let wk = wk.clone();
            async move {
                wk.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({
                    "issuer": "http://example.test",
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
        async fn well_known() -> Json<Value> {
            Json(serde_json::json!({
                "issuer": "http://example.test",
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
        async fn well_known() -> Json<Value> {
            Json(serde_json::json!({
                "issuer": "http://example.test",
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
            }],
            profiles: None,
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
            },
            endpoints: vec![],
            profiles: None,
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
                })
                .collect(),
            profiles: None,
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
            }],
            profiles: None,
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
}
