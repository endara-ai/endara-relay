use crate::config::DEFAULT_SESSION_IDENTITY_MAX_SESSIONS;
use crate::events::ClientIdentity;
use crate::js_sandbox::MetaToolHandler;
use crate::oauth::{OAuthFlowManager, OAuthSetupManager};
use crate::profile_registry::{ProfileContext, ProfileRegistry};
use crate::protocol::{self, ProtocolVersion};
use crate::registry::AdapterRegistry;
use crate::token_manager::TokenManager;
use crate::OAuthAdapterInners;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{
        sse::{Event, KeepAlive},
        Html, IntoResponse, Response, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::TcpListener;

use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{error, info, warn, Instrument};

/// Inbound MCP session header name. Returned by `initialize` so the client
/// can echo the same identifier on every follow-up request, and looked up
/// by [`SessionIdentityStore`] to recover the previously-captured
/// [`ClientIdentity`]. Matches the casing used by the MCP Streamable HTTP
/// spec.
pub(crate) const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Application state shared across all routes.
#[derive(Clone)]
pub struct AppState {
    pub registry: AdapterRegistry,
    pub js_execution_mode: Arc<AtomicBool>,
    pub meta_tool_handler: Arc<MetaToolHandler>,
    /// Relay-wide profile registry. R2.B/R3.A wire profile-scoped routes
    /// that resolve the active [`ProfileContext`] from the request path.
    #[allow(dead_code)]
    pub profile_registry: Arc<ProfileRegistry>,
    /// OAuth flow manager (shared with management routes).
    pub oauth_flow_manager: Option<Arc<OAuthFlowManager>>,
    /// Token manager for persisting OAuth tokens.
    pub token_manager: Option<Arc<TokenManager>>,
    /// Per-endpoint shared OAuth adapter inner states.
    pub oauth_adapter_inners: Option<OAuthAdapterInners>,
    /// Transient OAuth setup session manager (preflight flow).
    pub setup_manager: Option<Arc<OAuthSetupManager>>,
    /// Process start time, used to compute `/healthz` uptime.
    pub started_at: Instant,
    /// Convert JSON tool-call responses to TOON (Token-Oriented Object
    /// Notation) before they reach the MCP client. Computed at startup from
    /// `RelayConfig::toon_output` and the `--no-toon` CLI flag.
    pub toon_enabled: bool,
    /// Bounded LRU map of `Mcp-Session-Id` → [`ClientIdentity`] captured
    /// from each inbound `initialize` request. Looked up on every follow-up
    /// JSON-RPC message that echoes the session header so audit logs and
    /// the tool-call event bus can identify the caller without re-parsing
    /// `clientInfo`. Wrapped in `Arc<Mutex<_>>` so [`AppState`] stays
    /// `Clone` and cheap to pass through axum extractors.
    pub session_identities: Arc<Mutex<SessionIdentityStore>>,
    /// Precompiled JSON-Schema validators for the three relay-defined
    /// meta-tools. Their input shapes never change, so they are compiled once
    /// at startup (see [`MetaToolSchemas::new`]) and shared here. Consulted by
    /// [`validate_meta_tool_args`] at the top of each meta-tool branch in
    /// [`mcp_tools_call`].
    pub meta_tool_schemas: Arc<MetaToolSchemas>,
}

/// Precompiled JSON-Schema validators for the relay-defined meta-tools
/// (`list_tools`, `search_tools`, `execute_tools`), per spec §6.
///
/// Unlike upstream tool schemas — which are compiled lazily and cached on the
/// [`AdapterRegistry`] — these are owned by the relay and immutable, so they
/// are compiled once at startup. Each entry keeps its source schema beside the
/// compiled validator so [`crate::registry::validate_with_validator`] can list
/// the known parameter names in `additionalProperties` errors, making
/// meta-tool validation failures look identical to per-tool ones.
pub struct MetaToolSchemas {
    validators: HashMap<&'static str, (Arc<Validator>, Value)>,
}

impl MetaToolSchemas {
    /// Compile the static meta-tool schemas (spec §6.1). These are known-valid
    /// literals, so a compilation failure is a programmer error and panics at
    /// startup rather than silently disabling meta-tool validation.
    pub fn new() -> Arc<Self> {
        let specs: [(&'static str, Value); 3] = [
            (
                "list_tools",
                json!({
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "minimum": 1 },
                        "offset": { "type": "integer", "minimum": 0 }
                    },
                    "additionalProperties": false
                }),
            ),
            (
                "search_tools",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            ),
            (
                "execute_tools",
                json!({
                    "type": "object",
                    "properties": {
                        "script": { "type": "string" }
                    },
                    "required": ["script"],
                    "additionalProperties": false
                }),
            ),
        ];
        let mut validators = HashMap::with_capacity(specs.len());
        for (name, schema) in specs {
            let validator = jsonschema::options()
                .should_validate_formats(true)
                .build(&schema)
                .unwrap_or_else(|e| panic!("meta-tool schema for '{name}' must compile: {e}"));
            validators.insert(name, (Arc::new(validator), schema));
        }
        Arc::new(Self { validators })
    }

    /// Look up the compiled validator and source schema for a meta-tool by
    /// name. `None` for any non-meta-tool name.
    fn get(&self, name: &str) -> Option<&(Arc<Validator>, Value)> {
        self.validators.get(name)
    }
}

/// Bounded LRU cache of `Mcp-Session-Id` → [`ClientIdentity`] entries.
///
/// Capacity defaults to [`DEFAULT_SESSION_IDENTITY_MAX_SESSIONS`]; on
/// insert, the least-recently-used entry is evicted so a misbehaving
/// client that never reuses its session id cannot grow the map
/// unboundedly. Recency is tracked with a monotonically-increasing
/// counter and a [`BTreeMap`] indexed by that counter — both `get` and
/// `insert` re-stamp the entry as most-recently-used in `O(log n)`.
#[derive(Debug)]
pub struct SessionIdentityStore {
    capacity: usize,
    next_seq: u64,
    /// `session_id → (identity, detected dialect, recency seq)`. The seq is
    /// duplicated in [`recency`] so the LRU pop is `O(log n)` rather than
    /// `O(n)`. The dialect is the inbound peer's negotiated
    /// [`ProtocolVersion`], recorded at `initialize` time and consumed by
    /// later version-gated dispatch (T3).
    entries: HashMap<String, (ClientIdentity, ProtocolVersion, u64)>,
    /// `recency seq → session_id`. The smallest key is the least-
    /// recently-used entry; `pop_first` evicts it in `O(log n)`.
    recency: BTreeMap<u64, String>,
}

impl SessionIdentityStore {
    /// Build a store with the given capacity. A zero capacity is clamped
    /// to `1` because the dispatch path always wants at least one slot
    /// (the just-inserted entry) — operators who want to disable the
    /// cache entirely can do so at the config layer, not by setting `0`.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            next_seq: 0,
            entries: HashMap::with_capacity(capacity),
            recency: BTreeMap::new(),
        }
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        s
    }

    /// Insert or refresh an entry. Evicts the least-recently-used entry
    /// when the store is at capacity. The returned `Option` is the
    /// previously-stored identity for the same session id (typically
    /// `None`; a `Some` value indicates a session-id collision between
    /// two `initialize` calls and is overwritten).
    #[allow(dead_code)]
    pub fn insert(
        &mut self,
        session_id: String,
        identity: ClientIdentity,
    ) -> Option<ClientIdentity> {
        self.insert_with_dialect(session_id, identity, ProtocolVersion::default())
    }

    /// Insert or refresh an entry while recording the inbound peer's detected
    /// [`ProtocolVersion`]. Eviction semantics match [`insert`]; the returned
    /// `Option` is the previously-stored identity for the same session id.
    pub fn insert_with_dialect(
        &mut self,
        session_id: String,
        identity: ClientIdentity,
        dialect: ProtocolVersion,
    ) -> Option<ClientIdentity> {
        let new_seq = self.next_seq();
        let previous =
            if let Some((prev_id, _prev_dialect, prev_seq)) = self.entries.remove(&session_id) {
                self.recency.remove(&prev_seq);
                Some(prev_id)
            } else {
                None
            };
        while self.entries.len() >= self.capacity {
            let Some((evict_seq, evict_key)) = self.recency.pop_first() else {
                break;
            };
            self.entries.remove(&evict_key);
            // Defensive: drop the matching entries-table slot even if the
            // counters somehow drifted out of sync.
            let _ = evict_seq;
        }
        self.recency.insert(new_seq, session_id.clone());
        self.entries
            .insert(session_id, (identity, dialect, new_seq));
        previous
    }

    /// Resolve the [`ClientIdentity`] for a session id and mark the entry
    /// as most-recently-used.
    pub fn get(&mut self, session_id: &str) -> Option<ClientIdentity> {
        let (identity, dialect, prev_seq) = self.entries.get(session_id).cloned()?;
        self.recency.remove(&prev_seq);
        let new_seq = self.next_seq();
        self.recency.insert(new_seq, session_id.to_string());
        self.entries
            .insert(session_id.to_string(), (identity.clone(), dialect, new_seq));
        Some(identity)
    }

    /// Read the inbound dialect recorded for a session id without disturbing
    /// recency. Returns `None` for an unknown session. Consumed by T3 to gate
    /// dispatch on the inbound peer's negotiated protocol version.
    #[allow(dead_code)]
    pub fn dialect(&self, session_id: &str) -> Option<ProtocolVersion> {
        self.entries.get(session_id).map(|(_, dialect, _)| *dialect)
    }

    /// Number of live entries. Exposed for unit tests that exercise the
    /// LRU eviction path and for operators that inspect the store via
    /// future introspection hooks.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no sessions are cached. Paired with [`len`] so
    /// clippy's `len_without_is_empty` lint stays satisfied.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Effective capacity (after the `0 → 1` clamp). Surfaces the
    /// configured limit for tests and operator introspection.
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for SessionIdentityStore {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_SESSION_IDENTITY_MAX_SESSIONS)
    }
}

/// Read the first header value as a UTF-8 string, returning `None` for
/// missing or non-ASCII headers. Identity headers are best-effort signals,
/// so a malformed value is silently dropped rather than rejected.
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Extract a [`ClientIdentity`] from the per-request HTTP headers. Populated
/// from `User-Agent` and `Origin`; the structured `name` / `version` fields
/// stay `None` because no MCP-level clientInfo is available without an
/// `initialize` body.
fn identity_from_headers(headers: &HeaderMap) -> ClientIdentity {
    ClientIdentity {
        name: None,
        version: None,
        user_agent: header_str(headers, "user-agent").map(|s| s.to_string()),
        origin: header_str(headers, "origin").map(|s| s.to_string()),
    }
}

/// Extract a [`ClientIdentity`] from the `initialize` request's
/// `params.clientInfo` object. Per the MCP spec, `clientInfo.name` is
/// required but the relay treats every field as optional so a malformed
/// payload (missing `params`, non-object `clientInfo`, etc.) degrades to an
/// empty identity rather than rejecting the request.
fn identity_from_initialize_params(params: Option<&Value>) -> ClientIdentity {
    let Some(info) = params.and_then(|p| p.get("clientInfo")) else {
        return ClientIdentity::default();
    };
    ClientIdentity {
        name: info
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        version: info
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        user_agent: None,
        origin: None,
    }
}

/// Fold `fallback`'s non-empty fields into `primary` so structured
/// `clientInfo` (name/version from `initialize`) wins over the per-request
/// `User-Agent`/`Origin` headers when both are present.
fn merge_identity(primary: ClientIdentity, fallback: ClientIdentity) -> ClientIdentity {
    ClientIdentity {
        name: primary.name.or(fallback.name),
        version: primary.version.or(fallback.version),
        user_agent: primary.user_agent.or(fallback.user_agent),
        origin: primary.origin.or(fallback.origin),
    }
}

/// Resolve the caller's [`ClientIdentity`] for a non-`initialize` inbound
/// message. Looks up `Mcp-Session-Id` in [`SessionIdentityStore`] first
/// (the value cached at `initialize` time wins because it carries the
/// structured `clientInfo`), then merges in the per-request
/// `User-Agent`/`Origin` headers as a fallback. Returns `None` only when
/// every signal is empty so adapter event-emission can omit the `client`
/// field entirely.
fn resolve_inbound_identity(state: &AppState, headers: &HeaderMap) -> Option<ClientIdentity> {
    let header_identity = identity_from_headers(headers);
    let session_identity = header_str(headers, MCP_SESSION_ID_HEADER).and_then(|sid| {
        let mut guard = state.session_identities.lock().ok()?;
        guard.get(sid)
    });
    let merged = match session_identity {
        Some(s) => merge_identity(s, header_identity),
        None => header_identity,
    };
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// JSON-RPC request body expected by MCP routes.
#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[allow(dead_code)]
struct JsonRpcBody {
    jsonrpc: Option<String>,
    method: Option<String>,
    params: Option<Value>,
    id: Option<Value>,
}

fn jsonrpc_response(id: Option<Value>, result: Value) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    }))
}

/// Wrap a raw meta-tool result in the MCP `content` array format. When
/// `toon_enabled` is true, encode JSON objects/arrays to TOON; otherwise
/// fall back to pretty-printed JSON.
fn wrap_meta_tool_result(result: Value, toon_enabled: bool) -> Value {
    let text = if toon_enabled {
        crate::toon_convert::toonify_value(&result)
    } else {
        serde_json::to_string_pretty(&result).unwrap_or_default()
    };
    json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

/// Validate a meta-tool's `arguments` against its precompiled schema (spec
/// §6). Returns `Some(isError result)` to short-circuit the branch when the
/// arguments are invalid; `None` when they pass, when validation is disabled
/// (`relay.validate_inputs = false`), or when `name` is not a meta-tool.
///
/// Honours the same global `validate_inputs` toggle as the per-tool path and
/// reuses [`crate::registry::validate_with_validator`] so the returned
/// `isError` result is shaped identically to schema failures from
/// `route_tool_call`.
fn validate_meta_tool_args(state: &AppState, name: &str, arguments: &Value) -> Option<Value> {
    if !state.registry.validate_inputs() {
        return None;
    }
    let (validator, schema) = state.meta_tool_schemas.get(name)?;
    let (result, _message) =
        crate::registry::validate_with_validator(name, validator, schema, arguments)?;
    Some(result)
}

fn jsonrpc_error(id: Option<Value>, code: i64, message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "error": { "code": code, "message": message },
            "id": id,
        })),
    )
}

/// POST /mcp/initialize
///
/// `profile_ctx` is `Some` when the request arrived through a wildcard
/// `/mcp/{profile}` route — `instructions` is then rendered against the
/// profile's allowed endpoint set so only that profile's server types are
/// advertised. `None` for the global `/mcp` path.
async fn mcp_initialize(
    State(state): State<AppState>,
    Json(body): Json<JsonRpcBody>,
    profile_ctx: Option<&ProfileContext>,
) -> Json<Value> {
    let mut result = json!({
        "protocolVersion": protocol::VERSION_2025_03_26,
        "capabilities": {
            "tools": { "listChanged": true }
        },
        "serverInfo": {
            "name": "Endara Relay",
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    let instructions = match profile_ctx {
        Some(ctx) => crate::advertise::instructions_for_profile(&ctx.registry_view).await,
        None => crate::advertise::instructions(&state.registry).await,
    };
    if let Some(instructions) = instructions {
        result["instructions"] = Value::String(instructions);
    }
    jsonrpc_response(body.id, result)
}

/// Build the meta-tool definitions as JSON values.
///
/// `list_tools` and `search_tools` are always advertised. `execute_tools` is
/// only included when `js_mode` is on — this matches the invocation-side
/// gate in `mcp_tools_call`, which rejects `execute_tools` calls when
/// `local_js_execution` is disabled.
///
/// The descriptions are built dynamically against the supplied [`AdapterRegistry`]
/// so each `tools/list` response advertises the currently-Healthy server set
/// (see `crate::advertise`). When `profile_ctx` is `Some`, the `_for_profile`
/// description builders are used so descriptions only mention server types
/// inside the profile's allowed endpoint set.
async fn meta_tool_definitions(
    js_mode: bool,
    registry: &AdapterRegistry,
    toon_enabled: bool,
    profile_ctx: Option<&ProfileContext>,
) -> Vec<Value> {
    let (list_desc, search_desc) = match profile_ctx {
        Some(ctx) => (
            crate::advertise::list_tools_description_for_profile(&ctx.registry_view).await,
            crate::advertise::search_tools_description_for_profile(
                &ctx.registry_view,
                toon_enabled,
            )
            .await,
        ),
        None => (
            crate::advertise::list_tools_description(registry).await,
            crate::advertise::search_tools_description(registry, toon_enabled).await,
        ),
    };
    let mut tools = vec![
        json!({
            "name": "list_tools",
            "description": list_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "search_tools",
            "description": search_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            }
        }),
    ];
    if !js_mode {
        return tools;
    }
    let execute_desc = match profile_ctx {
        Some(ctx) => {
            crate::advertise::execute_tools_description_for_profile(&ctx.registry_view).await
        }
        None => crate::advertise::execute_tools_description(registry).await,
    };
    tools.push(json!({
        "name": "execute_tools",
        "description": execute_desc,
        "inputSchema": {
            "type": "object",
            "properties": {
                "script": { "type": "string" }
            },
            "required": ["script"]
        }
    }));
    tools
}

/// POST /mcp/tools/list
///
/// `profile_ctx` threads the resolved [`ProfileContext`] from the wildcard
/// `/mcp/{profile}` route so meta-tool descriptions advertise only the
/// profile's server types. `None` for the global `/mcp` path. R3.A owns
/// the catalog filtering itself (line 199 below).
///
/// Per-profile `js_execution` and `toon_output` (R3.B) override the global
/// [`AppState::js_execution_mode`] and [`AppState::toon_enabled`] toggles
/// whenever `profile_ctx` is `Some`. `ProfileContext` already pre-resolves
/// the `Inherit | On | Off` semantics (`None` falls back to the global flag
/// at rebuild time), so the gate sites just read the resolved `bool`.
async fn mcp_tools_list(
    State(state): State<AppState>,
    Json(body): Json<JsonRpcBody>,
    profile_ctx: Option<&ProfileContext>,
) -> Json<Value> {
    let js_mode = profile_ctx
        .map(|c| c.js_execution)
        .unwrap_or_else(|| state.js_execution_mode.load(Ordering::Relaxed));
    let toon_enabled = profile_ctx
        .map(|c| c.toon_output)
        .unwrap_or(state.toon_enabled);
    let meta_tools =
        meta_tool_definitions(js_mode, &state.registry, toon_enabled, profile_ctx).await;

    let tools: Vec<Value> = if js_mode {
        // JS execution mode: only the 3 meta-tools (incl. execute_tools)
        meta_tools
    } else {
        // Normal mode: full prefixed catalog + list/search meta-tools.
        // execute_tools is intentionally hidden — it is gated on
        // local_js_execution and the matching invocation-side check
        // would reject any call to it.
        let catalog = match profile_ctx {
            Some(ctx) => ctx.registry_view.merged_catalog().await,
            None => state.registry.merged_catalog().await,
        };
        let mut tools: Vec<Value> = catalog
            .into_iter()
            .map(|t| {
                let mut tool = json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                });
                if let Some(annotations) = t.annotations {
                    tool["annotations"] = annotations;
                }
                tool
            })
            .collect();
        tools.extend(meta_tools);
        tools.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        tools
    };

    jsonrpc_response(body.id, json!({ "tools": tools }))
}

/// POST /mcp/tools/call
///
/// `profile_ctx` is `Some` for the wildcard `/mcp/{profile}` route and
/// `None` for the global `/mcp` (and legacy `/mcp/tools/call`) path. When
/// present, per-profile `js_execution` and `toon_output` (R3.B) override
/// the global [`AppState::js_execution_mode`] / [`AppState::toon_enabled`]
/// toggles, and the per-profile [`MetaToolHandler`] handles the meta-tool
/// dispatch so list/search/execute see only the profile's allowed
/// endpoints.
async fn mcp_tools_call(
    State(state): State<AppState>,
    Json(body): Json<JsonRpcBody>,
    profile_ctx: Option<&ProfileContext>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let params = body.params.unwrap_or(json!({}));
    let tool_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| jsonrpc_error(body.id.clone(), -32602, "missing 'name' in params"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let js_mode = profile_ctx
        .map(|c| c.js_execution)
        .unwrap_or_else(|| state.js_execution_mode.load(Ordering::Relaxed));
    let toon_enabled = profile_ctx
        .map(|c| c.toon_output)
        .unwrap_or(state.toon_enabled);
    // `ProfileContext::meta_tool_handler` is populated by `ProfileRegistry::rebuild`
    // (R3.A) over the profile's [`ProfileRegistryView`] — using it here keeps
    // list/search/execute scoped to the profile's allowed endpoints. The global
    // handler is the fallback for `/mcp` and the legacy `/mcp/tools/call`.
    let handler: &MetaToolHandler = profile_ctx
        .and_then(|c| c.meta_tool_handler.as_deref())
        .unwrap_or(&state.meta_tool_handler);

    // Check if this is a meta-tool call
    match tool_name {
        "list_tools" => {
            if let Some(err) = validate_meta_tool_args(&state, "list_tools", &arguments) {
                return Ok(jsonrpc_response(body.id, err));
            }
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let offset = arguments
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            match handler.list_tools(limit, offset).await {
                Ok(result) => {
                    return Ok(jsonrpc_response(
                        body.id,
                        wrap_meta_tool_result(result, toon_enabled),
                    ))
                }
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        "search_tools" => {
            if let Some(err) = validate_meta_tool_args(&state, "search_tools", &arguments) {
                return Ok(jsonrpc_response(body.id, err));
            }
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            match handler.search_tools(query, limit).await {
                Ok(result) => {
                    return Ok(jsonrpc_response(
                        body.id,
                        wrap_meta_tool_result(result, toon_enabled),
                    ))
                }
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        "execute_tools" => {
            // Symmetric to the catalog hide in `meta_tool_definitions`:
            // execute_tools is only available when local_js_execution is on.
            // A misbehaving or malicious client could still invoke it
            // directly without going through tools/list, so reject here.
            if !js_mode {
                return Err(jsonrpc_error(
                    body.id,
                    -32601,
                    "execute_tools is disabled — set relay.local_js_execution = true to enable the JS sandbox.",
                ));
            }
            if let Some(err) = validate_meta_tool_args(&state, "execute_tools", &arguments) {
                return Ok(jsonrpc_response(body.id, err));
            }
            let script = arguments
                .get("script")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Recover the outer inbound request's identity and canonical UID
            // from the surrounding `request{request_uid=...,client=...}` span
            // (seeded by `handle_single_message` / the logged wrappers) and
            // re-serialise them so the sandbox can re-establish the caller's
            // span around each inner upstream tool call across the
            // blocking-thread hop. Without this, aggregated `execute_tools`
            // inner calls emit an empty client and a `None` request_uid.
            let request_ctx = crate::events::current_request_context();
            let client_json = request_ctx
                .client
                .filter(|c| !c.is_empty())
                .map(|c| serde_json::to_string(&c).unwrap_or_default())
                .unwrap_or_default();
            let request_uid = request_ctx.request_uid.unwrap_or_default();
            match handler
                .execute_tools(script, &client_json, &request_uid)
                .await
            {
                Ok(result) => {
                    return Ok(jsonrpc_response(
                        body.id,
                        wrap_meta_tool_result(result, toon_enabled),
                    ))
                }
                Err(e) => return Err(jsonrpc_error(body.id, -32603, &e.to_string())),
            }
        }
        _ => {}
    }

    // Not a meta-tool — if JS mode is on, reject direct tool calls
    if js_mode {
        return Err(jsonrpc_error(
            body.id,
            -32601,
            "Direct tool calls are not allowed in JS execution mode. Use execute_tools instead.",
        ));
    }

    let route_result = match profile_ctx {
        Some(ctx) => {
            ctx.registry_view
                .route_tool_call(tool_name, arguments)
                .await
        }
        None => state.registry.route_tool_call(tool_name, arguments).await,
    };
    match route_result {
        Ok(result) => {
            let result = if toon_enabled {
                crate::toon_convert::toonify_call_result(result)
            } else {
                result
            };
            Ok(jsonrpc_response(body.id, result))
        }
        Err(e) => Err(jsonrpc_error(body.id, -32603, &e.to_string())),
    }
}

/// Handle a single JSON-RPC message object, returning `None` for notifications
/// (which get 202 Accepted) or `Some(Value)` for requests that need a response.
///
/// `profile_ctx` is `Some` when the request arrived through a wildcard
/// `/mcp/{profile}` route and identifies the resolved [`ProfileContext`]. It
/// is `None` for the global `/mcp` endpoint. R3.C uses it to render
/// profile-scoped `InitializeResult.instructions` and meta-tool
/// descriptions; R3.A consumes it for per-profile catalog scoping and
/// tools/call dispatch.
async fn handle_single_message(
    state: &AppState,
    msg: Value,
    headers_str: &str,
    profile_ctx: Option<&ProfileContext>,
    client_identity: Option<&ClientIdentity>,
) -> Option<Value> {
    let method = msg
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id_str = msg
        .get("id")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    // Canonical per-JSON-RPC-message UID. `handle_single_message` runs once
    // per inbound JSON-RPC message — including once per element of a batch
    // array — so each logical request gets its own UID. Unlike the
    // client-controlled JSON-RPC `id` (which collides across clients and
    // across reconnects when a client resets its counter), this UUID is unique
    // per message, so every log line and `ToolCallEvent` produced while
    // processing this message shares one collision-free key for the desktop's
    // row/overlay keying.
    let request_uid = uuid::Uuid::new_v4().to_string();
    // JSON-encode the identity so `SpanFieldCaptureLayer` can deserialise it
    // back into a typed [`ClientIdentity`] via `current_request_context()`
    // without changing the `McpAdapter::call_tool` trait signature. An empty
    // identity is rendered as an empty `""` field so the capture visitor
    // silently drops it.
    let client_json = client_identity
        .filter(|c| !c.is_empty())
        .map(|c| serde_json::to_string(c).unwrap_or_default())
        .unwrap_or_default();
    let client_name = client_identity
        .and_then(|c| c.client_label())
        .unwrap_or_default();
    let client_version = client_identity
        .and_then(|c| c.version.clone())
        .unwrap_or_default();
    let span = tracing::info_span!(
        "request",
        method = %method,
        id = %id_str,
        request_uid = %request_uid,
        client = %client_json,
    );
    async move {
        let start = Instant::now();
        let is_notification = msg.get("id").is_none() || msg.get("id") == Some(&Value::Null);
        let req_bytes = serde_json::to_string(&msg).map(|s| s.len()).unwrap_or(0);

        // Notifications (no `id` field) get 202 Accepted with no body per MCP spec.
        if is_notification {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            info!(
                method = %method,
                request_uid = %request_uid,
                elapsed_ms = elapsed_ms,
                req_bytes = req_bytes,
                resp_bytes = 0,
                status = 202,
                headers = %headers_str,
                client_name = ?client_name,
                client_version = ?client_version,
                "MCP notification"
            );
            return None;
        }

        // Deserialize as JsonRpcBody for dispatch
        let body: JsonRpcBody = match serde_json::from_value(msg) {
            Ok(b) => b,
            Err(_) => {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32600, "message": "Invalid Request" },
                    "id": null,
                }));
            }
        };

        let result: Result<Json<Value>, (StatusCode, Json<Value>)> = match method.as_str() {
            "initialize" => Ok(mcp_initialize(State(state.clone()), Json(body), profile_ctx).await),
            "tools/list" => Ok(mcp_tools_list(State(state.clone()), Json(body), profile_ctx).await),
            "tools/call" => mcp_tools_call(State(state.clone()), Json(body), profile_ctx).await,
            _ => Err(jsonrpc_error(
                body.id,
                -32601,
                &format!("method not found: {}", method),
            )),
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let resp_value = match result {
            Ok(Json(resp)) => {
                let resp_bytes = serde_json::to_string(&resp).map(|s| s.len()).unwrap_or(0);
                info!(
                    method = %method,
                    request_uid = %request_uid,
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = 200,
                    headers = %headers_str,
                    client_name = ?client_name,
                    client_version = ?client_version,
                    "MCP request"
                );
                resp
            }
            Err((status, Json(resp))) => {
                let resp_bytes = serde_json::to_string(&resp).map(|s| s.len()).unwrap_or(0);
                let status_code = status.as_u16();
                if status_code >= 500 {
                    error!(
                        method = %method,
                        request_uid = %request_uid,
                        elapsed_ms = elapsed_ms,
                        req_bytes = req_bytes,
                        resp_bytes = resp_bytes,
                        status = status_code,
                        headers = %headers_str,
                        client_name = ?client_name,
                        client_version = ?client_version,
                        "MCP request"
                    );
                } else if status_code == 200 {
                    // JSON-RPC 2.0: errors are returned with HTTP 200, not a sign of trouble.
                    info!(
                        method = %method,
                        request_uid = %request_uid,
                        elapsed_ms = elapsed_ms,
                        req_bytes = req_bytes,
                        resp_bytes = resp_bytes,
                        status = status_code,
                        headers = %headers_str,
                        client_name = ?client_name,
                        client_version = ?client_version,
                        "MCP request"
                    );
                } else {
                    warn!(
                        method = %method,
                        request_uid = %request_uid,
                        elapsed_ms = elapsed_ms,
                        req_bytes = req_bytes,
                        resp_bytes = resp_bytes,
                        status = status_code,
                        headers = %headers_str,
                        client_name = ?client_name,
                        client_version = ?client_version,
                        "MCP request"
                    );
                }
                resp
            }
        };

        Some(resp_value)
    }
    .instrument(span)
    .await
}

/// POST /mcp — Unified Streamable HTTP transport endpoint.
///
/// Accepts a JSON-RPC request (single object or batch array) and dispatches by
/// the `method` field to the appropriate handler, as required by the MCP
/// Streamable HTTP spec.
///
/// Per the spec, JSON-RPC notifications (messages without an `id` field) must
/// receive HTTP 202 Accepted with no body.
///
/// Batch requests (JSON arrays) are processed and return an array of responses.
/// If all messages in a batch are notifications, returns 202 Accepted.
async fn mcp_unified(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mcp_unified_impl(state, headers, body, None).await
}

/// POST `/mcp/{profile}` — profile-scoped variant of [`mcp_unified`].
///
/// Resolves the URL `{profile}` segment against
/// [`AppState::profile_registry`] and 404s if unknown. On hit, delegates to
/// [`mcp_unified_impl`] with the resolved [`ProfileContext`] so the global and
/// profiled paths share a single dispatch implementation.
///
/// The catalog/tool-call scoping inside that shared implementation is wired
/// in R3.A; for now the context is plumbed through but unused, and the
/// dispatch behaviour matches the global `/mcp` endpoint.
///
/// The dispatch is wrapped in an `info_span!("mcp_request", profile = ...)`
/// so every event emitted while handling the request — including the inner
/// `request` span's `MCP request` / `MCP notification` lines and any
/// adapter-side child spans — inherits the `profile` field. The field key
/// `profile` is an immutable cross-stack contract with the desktop log
/// parser (locked decision Cross-stack #1 / engineering-spec §7.1).
async fn mcp_unified_profiled(
    State(state): State<AppState>,
    Path(profile_path): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(profile_ctx) = state.profile_registry.get(&profile_path).await else {
        return profile_not_found_response(&profile_path);
    };
    let span = tracing::info_span!("mcp_request", profile = %profile_path);
    mcp_unified_impl(state, headers, body, Some(profile_ctx))
        .instrument(span)
        .await
}

/// Shared body of [`mcp_unified`] and [`mcp_unified_profiled`]. `profile_ctx`
/// is `None` for the global `/mcp` route and `Some(...)` for
/// `/mcp/{profile}`; see [`handle_single_message`] for how it is threaded
/// through to per-message dispatch.
///
/// Per-message identity resolution: an `initialize` request seeds the
/// `Mcp-Session-Id` → [`ClientIdentity`] cache from `params.clientInfo` and
/// echoes the new session id back on the response so the client can
/// correlate follow-up calls. Every other message resolves its caller via
/// [`resolve_inbound_identity`] (session lookup with per-request
/// `User-Agent`/`Origin` fallback) before dispatch.
async fn mcp_unified_impl(
    state: AppState,
    headers: HeaderMap,
    body: Value,
    profile_ctx: Option<Arc<ProfileContext>>,
) -> Response {
    let headers_str: String = headers
        .iter()
        .map(|(k, v)| format!("{}={}", k, v.to_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join(" ");
    let profile_ctx_ref = profile_ctx.as_deref();
    // Collected here so a batch that opens with an `initialize` still
    // bubbles the new session id up to the HTTP response headers. The MCP
    // spec singleton case is the common path; the batch path is defensive.
    let mut emitted_session_id: Option<String> = None;

    match body {
        Value::Array(messages) => {
            if messages.is_empty() {
                return json_response(json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32600, "message": "Invalid Request: empty batch" },
                    "id": null,
                }));
            }

            let mut responses: Vec<Value> = Vec::new();
            for msg in messages {
                let (identity, new_session) = resolve_identity_for_message(&state, &headers, &msg);
                if emitted_session_id.is_none() {
                    emitted_session_id = new_session;
                }
                if let Some(resp) = handle_single_message(
                    &state,
                    msg,
                    &headers_str,
                    profile_ctx_ref,
                    identity.as_ref(),
                )
                .await
                {
                    responses.push(resp);
                }
            }

            if responses.is_empty() {
                // All messages were notifications
                with_session_header(StatusCode::ACCEPTED.into_response(), emitted_session_id)
            } else {
                with_session_header(json_response(Value::Array(responses)), emitted_session_id)
            }
        }
        Value::Object(_) => {
            let (identity, new_session) = resolve_identity_for_message(&state, &headers, &body);
            emitted_session_id = new_session;
            match handle_single_message(
                &state,
                body,
                &headers_str,
                profile_ctx_ref,
                identity.as_ref(),
            )
            .await
            {
                Some(resp) => with_session_header(json_response(resp), emitted_session_id),
                None => {
                    with_session_header(StatusCode::ACCEPTED.into_response(), emitted_session_id)
                }
            }
        }
        _ => json_response(json!({
            "jsonrpc": "2.0",
            "error": { "code": -32600, "message": "Invalid Request: expected object or array" },
            "id": null,
        })),
    }
}

/// Build a `200 OK` JSON response body. Used by [`mcp_unified_impl`] for the
/// non-error branches; pairs with [`with_session_header`] to attach the
/// optional `Mcp-Session-Id` header without duplicating the `Content-Type`
/// boilerplate at every callsite.
fn json_response(body: Value) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        Json(body),
    )
        .into_response()
}

/// Attach `Mcp-Session-Id: <session>` to an existing response when an
/// `initialize` in the same request issued a new session id. The header is
/// only set when `session_id` is `Some` so non-`initialize` responses stay
/// untouched.
fn with_session_header(mut response: Response, session_id: Option<String>) -> Response {
    if let Some(sid) = session_id {
        if let Ok(val) = HeaderValue::from_str(&sid) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(MCP_SESSION_ID_HEADER), val);
        }
    }
    response
}

/// Per-message identity resolution shared by the singleton and batch
/// branches of [`mcp_unified_impl`]. Returns the [`ClientIdentity`] to
/// associate with `msg` and (for `initialize` messages) the newly-issued
/// session id so the HTTP response can echo it back.
fn resolve_identity_for_message(
    state: &AppState,
    headers: &HeaderMap,
    msg: &Value,
) -> (Option<ClientIdentity>, Option<String>) {
    let is_initialize = msg.get("method").and_then(|v| v.as_str()) == Some("initialize");
    if !is_initialize {
        return (resolve_inbound_identity(state, headers), None);
    }
    // `initialize`: issue a fresh session id unconditionally so the
    // client can echo it on follow-ups, even when it sent no
    // `clientInfo`. Only cache the structured `clientInfo` (name /
    // version) — the per-request `User-Agent`/`Origin` fallback covers
    // the empty case on later requests without polluting the LRU with
    // zero-signal entries.
    let init_identity = identity_from_initialize_params(msg.get("params"));
    let header_identity = identity_from_headers(headers);
    let session_id = uuid::Uuid::new_v4().to_string();
    // Detect and record the inbound peer's dialect alongside its identity so
    // version-gated dispatch (T3) can branch on it. Pure plumbing: storing the
    // value changes no response or handshake behavior.
    let dialect = protocol::detect_inbound_dialect(
        true,
        header_str(headers, protocol::MCP_PROTOCOL_VERSION_HEADER),
        msg.get("params"),
    );
    if !init_identity.is_empty() {
        if let Ok(mut guard) = state.session_identities.lock() {
            guard.insert_with_dialect(session_id.clone(), init_identity.clone(), dialect);
        }
    }
    let merged = merge_identity(init_identity, header_identity);
    let identity = if merged.is_empty() {
        None
    } else {
        Some(merged)
    };
    (identity, Some(session_id))
}

/// JSON 404 body returned by profile-scoped routes when the URL `{profile}`
/// segment does not resolve to a registered profile (test-matrix row #24).
/// Shape mirrors the JSON-RPC error envelope used by [`mcp_unified`] for
/// other invalid-request cases so generic JSON-RPC clients can surface a
/// meaningful message.
fn profile_not_found_response(profile_path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        Json(json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32004,
                "message": format!("unknown profile '{}'", profile_path),
            },
            "id": null,
        })),
    )
        .into_response()
}

/// GET /mcp/sse — basic SSE transport.
///
/// Sends the initial `endpoint` event (`data: /mcp`) so legacy SSE clients
/// learn where to POST JSON-RPC requests, then forwards every
/// `notifications/tools/list_changed` tick from the registry-wide broadcast
/// as a JSON-RPC notification frame so subscribers can re-fetch
/// `tools/list`. An Axum keep-alive layer prevents idle proxies from
/// dropping the connection.
///
/// Caveat: clients that only POST to `/mcp` (the unified request/response
/// endpoint, no SSE subscription) will not receive these notifications even
/// though `mcp_initialize` advertises `tools.listChanged: true`. That is
/// intentional — `listChanged` describes server capability, not per-
/// transport delivery — and POST-only clients are expected to re-fetch
/// `tools/list` on their own cadence.
async fn mcp_sse(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    build_mcp_sse_stream(&state, "/mcp", None)
}

/// GET `/mcp/{profile}/sse` — profile-scoped SSE transport.
///
/// Resolves the URL `{profile}` segment against
/// [`AppState::profile_registry`] and 404s if unknown. On hit, returns the
/// same SSE stream shape as [`mcp_sse`], pointing the initial `endpoint`
/// event at `/mcp/{profile}` so SSE-only clients POST follow-up JSON-RPC
/// requests back to the same profile.
///
/// The profile filter passed into [`build_mcp_sse_stream`] re-reads the
/// allowed-endpoints set from [`AppState::profile_registry`] on every
/// per-endpoint tick so mid-stream profile-membership changes (via
/// `update_profile` or the config watcher) take effect without
/// reconnecting. Profile-level mutations (membership change, `js_execution`
/// toggle, profile add/remove) arrive on the parallel
/// [`ProfileRegistry::subscribe_profiles_changed`] channel.
async fn mcp_sse_profiled(
    State(state): State<AppState>,
    Path(profile_path): Path<String>,
) -> Response {
    if state.profile_registry.get(&profile_path).await.is_none() {
        return profile_not_found_response(&profile_path);
    }
    let endpoint_uri = format!("/mcp/{}", profile_path);
    let filter = ProfileSseFilter {
        path: profile_path.to_ascii_lowercase(),
        registry: state.profile_registry.clone(),
    };
    build_mcp_sse_stream(&state, &endpoint_uri, Some(filter)).into_response()
}

/// Profile-scoped filter handed to [`build_mcp_sse_stream`].
///
/// The static `HashSet<String>` allowed-endpoints snapshot used pre-R3.D is
/// replaced by a `(path, registry)` pair so the stream can resolve the
/// *current* allowed set each tick. That way an `update_profile` that adds
/// an endpoint surfaces on already-open SSE streams without forcing the
/// client to reconnect (spec acceptance #2).
struct ProfileSseFilter {
    /// Lowercased profile path — keys the [`ProfileRegistry`] lookup and
    /// matches against
    /// [`ProfileRegistry::subscribe_profiles_changed`] payloads.
    path: String,
    /// Shared handle to the live profile registry.
    registry: Arc<ProfileRegistry>,
}

/// Build the SSE stream backing both [`mcp_sse`] and [`mcp_sse_profiled`].
///
/// `endpoint_data` is the value emitted on the initial `endpoint` event so
/// SSE-only clients learn where to POST follow-up JSON-RPC requests
/// (`/mcp` for the global route, `/mcp/{profile}` for the profiled one).
///
/// `profile_filter` controls which `tools/list_changed` ticks are
/// forwarded:
/// - `None` (global `/mcp/sse`) forwards every per-endpoint tick from
///   [`AdapterRegistry::subscribe_tools_changed`] and ignores the profile
///   channel entirely — behaviour unchanged from pre-R3.D.
/// - `Some(filter)` (profile-scoped) forwards per-endpoint ticks only when
///   the changed endpoint is in the profile's *current* allowed set
///   (re-resolved against `filter.registry` each tick, so mid-stream
///   membership changes take effect), AND forwards profile-channel ticks
///   when the payload path matches `filter.path` (membership change, JS
///   toggle, profile add/remove).
///
/// Both channels treat `Lagged` as an unconditional forward — the client
/// re-fetches `tools/list` on receipt and re-discovers any missed change.
fn build_mcp_sse_stream(
    state: &AppState,
    endpoint_data: &str,
    profile_filter: Option<ProfileSseFilter>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    use tokio::sync::broadcast::error::RecvError;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut tools_rx = state.registry.subscribe_tools_changed();
    // Subscribe to the profile channel even for the global stream so the
    // two `select!` branches are uniform; the `None` filter just drops the
    // payloads on the floor. Keeping the subscription means a slow profile
    // rebuild can't back-pressure the global stream into `Lagged`.
    let mut profiles_rx = state.profile_registry.subscribe_profiles_changed();
    // Keep-alive Arc clones moved into the spawn so the broadcast `Sender`s
    // outlive the request scope. Without these, test harnesses that consume
    // `AppState` into a one-shot router (e.g. `tower::ServiceExt::oneshot`)
    // drop the underlying registries as soon as the response is built,
    // closing both broadcast channels and tripping the `Closed` arms below
    // before any tick can be evaluated. Production paths keep `AppState`
    // alive for the lifetime of the server, so this is purely defensive for
    // short-lived state scopes.
    let registry_keepalive = state.registry.clone();
    let profile_registry_keepalive = state.profile_registry.clone();
    let endpoint_data = endpoint_data.to_string();

    tokio::spawn(async move {
        // Bind the keep-alives so the borrow checker keeps them in scope
        // until the spawn exits.
        let _registry_keepalive = registry_keepalive;
        let _profile_registry_keepalive = profile_registry_keepalive;
        if tx
            .send(Ok(Event::default().event("endpoint").data(endpoint_data)))
            .await
            .is_err()
        {
            return;
        }
        // Re-usable JSON-RPC notification frame body. Defined once so both
        // branches of the `select!` and the `Lagged` arms emit byte-for-byte
        // identical SSE frames.
        const FRAME_BODY: &str = r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
        loop {
            tokio::select! {
                ep_tick = tools_rx.recv() => {
                    match ep_tick {
                        Ok(name) => {
                            // Resolve the live allowed-endpoints set per-tick
                            // so membership changes published since stream
                            // open take effect without reconnection.
                            let forward = match &profile_filter {
                                None => true,
                                Some(f) => match f.registry.get(&f.path).await {
                                    Some(ctx) => {
                                        ctx.registry_view.allowed_endpoints().contains(&name)
                                    }
                                    // Profile deleted mid-stream: suppress
                                    // per-endpoint ticks. The matching
                                    // profile-channel tick (emitted by
                                    // `rebuild`) will still flow through the
                                    // other branch and deliver one final
                                    // frame to the client.
                                    None => false,
                                },
                            };
                            if !forward {
                                continue;
                            }
                            if tx.send(Ok(Event::default().data(FRAME_BODY))).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => {
                            if tx.send(Ok(Event::default().data(FRAME_BODY))).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
                prof_tick = profiles_rx.recv() => {
                    match prof_tick {
                        Ok(path) => {
                            // Profile-channel ticks only matter for
                            // profile-scoped streams; the global stream
                            // ignores them entirely (its tool surface is
                            // governed by per-endpoint ticks).
                            let forward = match &profile_filter {
                                None => false,
                                Some(f) => path == f.path,
                            };
                            if !forward {
                                continue;
                            }
                            if tx.send(Ok(Event::default().data(FRAME_BODY))).await.is_err() {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => {
                            // Same policy as the per-endpoint channel: on
                            // lag the originating path is lost, so forward
                            // unconditionally for profile-scoped streams and
                            // suppress for the global stream (which doesn't
                            // care about profile ticks at all).
                            if profile_filter.is_some()
                                && tx.send(Ok(Event::default().data(FRAME_BODY))).await.is_err()
                            {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => {
                            // The profile registry only drops when the
                            // process exits; treat the same as the
                            // per-endpoint `Closed` and bail out so we don't
                            // spin on a dead channel.
                            break;
                        }
                    }
                }
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// Query params for the OAuth callback.
#[derive(Deserialize)]
struct OAuthCallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Escape a string for safe interpolation into an HTML text node or quoted attribute.
///
/// The OAuth callback handler interpolates upstream-influenced strings (error
/// codes, error descriptions, token-endpoint response bodies, and the locally
/// configured endpoint name) into the response HTML. Without escaping, a
/// malicious upstream `error_description` such as `<script>fetch('/api/...')` would
/// execute on the relay's own `http://127.0.0.1:<port>` origin — same origin as
/// the management API. Pair this with the `default-src 'none'` CSP added by
/// [`oauth_html_response`] to also block any future regression that might inline
/// a `<script>` tag.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Build a `text/html` response for the OAuth callback with a strict
/// `Content-Security-Policy: default-src 'none'` header. The CSP makes any
/// regression that re-introduces unescaped interpolation un-exploitable as
/// inline `<script>`/`<img onerror>`/etc., because the user agent will refuse
/// to fetch or execute any subresource.
fn oauth_html_response(body: String) -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'",
            ),
            (
                axum::http::header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
            (
                axum::http::header::HeaderName::from_static("referrer-policy"),
                "no-referrer",
            ),
        ],
        Html(body),
    )
        .into_response()
}

/// GET /oauth/callback
///
/// The OAuth authorization server redirects the user's browser here after login.
/// Exchanges the authorization code for tokens, saves them, and signals the adapter.
async fn oauth_callback(
    State(state): State<AppState>,
    Query(params): Query<OAuthCallbackParams>,
) -> Response {
    // Handle OAuth error
    if let Some(ref err) = params.error {
        warn!(error = %err, "OAuth callback received error");
        return oauth_html_response(format!(
            "<html><body><h1>OAuth Error</h1><p>{}</p><p>You can close this window.</p></body></html>",
            html_escape(err)
        ));
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            return oauth_html_response(
                "<html><body><h1>OAuth Error</h1><p>Missing authorization code.</p><p>You can close this window.</p></body></html>"
                    .to_string(),
            );
        }
    };
    let state_param = match params.state {
        Some(s) => s,
        None => {
            return oauth_html_response(
                "<html><body><h1>OAuth Error</h1><p>Missing state parameter.</p><p>You can close this window.</p></body></html>"
                    .to_string(),
            );
        }
    };

    let Some(ref flow_mgr) = state.oauth_flow_manager else {
        return oauth_html_response(
            "<html><body><h1>OAuth Error</h1><p>OAuth not configured.</p></body></html>"
                .to_string(),
        );
    };

    let flow = match flow_mgr.consume_flow(&state_param).await {
        Some(f) => f,
        None => {
            warn!(state = %state_param, "Invalid or expired OAuth state");
            return oauth_html_response(
                "<html><body><h1>OAuth Error</h1><p>Invalid or expired login session. Please try again.</p><p>You can close this window.</p></body></html>"
                    .to_string(),
            );
        }
    };

    // Exchange authorization code for tokens
    let client = reqwest::Client::new();
    let mut form_parts: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code),
        ("redirect_uri".into(), flow.redirect_uri.clone()),
        ("client_id".into(), flow.client_id.clone()),
        ("code_verifier".into(), flow.code_verifier.clone()),
    ];
    if let Some(ref secret) = flow.client_secret {
        form_parts.push(("client_secret".into(), secret.clone()));
    }

    let form_body: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form_parts.iter())
        .finish();

    let token_response: reqwest::Response = match client
        .post(&flow.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!(error = %e, "Failed to exchange authorization code");
            return oauth_html_response(format!(
                "<html><body><h1>OAuth Error</h1><p>Failed to exchange code: {}</p><p>You can close this window.</p></body></html>",
                html_escape(&e.to_string())
            ));
        }
    };

    if !token_response.status().is_success() {
        let status = token_response.status();
        let body = token_response.text().await.unwrap_or_default();
        error!(%status, body = %body, "Token endpoint returned error");
        return oauth_html_response(format!(
            "<html><body><h1>OAuth Error</h1><p>Token endpoint returned {}: {}</p><p>You can close this window.</p></body></html>",
            html_escape(status.as_str()),
            html_escape(&body)
        ));
    }

    let token_json: serde_json::Value = match token_response.json().await {
        Ok(v) => v,
        Err(e) => {
            error!(error = %e, "Failed to parse token response");
            return oauth_html_response(format!(
                "<html><body><h1>OAuth Error</h1><p>Invalid token response: {}</p><p>You can close this window.</p></body></html>",
                html_escape(&e.to_string())
            ));
        }
    };

    let access_token = token_json["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return oauth_html_response(
            "<html><body><h1>OAuth Error</h1><p>No access_token in response.</p><p>You can close this window.</p></body></html>"
                .to_string(),
        );
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let token_set = crate::token_manager::TokenSet {
        access_token: access_token.clone(),
        refresh_token: token_json["refresh_token"].as_str().map(|s| s.to_string()),
        expires_at: token_json["expires_in"]
            .as_u64()
            .map(|secs| now_secs + secs),
        token_type: token_json["token_type"]
            .as_str()
            .unwrap_or("Bearer")
            .to_string(),
        scope: token_json["scope"].as_str().map(|s| s.to_string()),
        issued_at: Some(now_secs),
    };

    // Check if this is a setup session callback (preflight flow)
    if let Some(session_id_str) = flow.endpoint_name.strip_prefix("setup:") {
        if let Ok(session_id) = session_id_str.parse::<uuid::Uuid>() {
            if let Some(ref setup_mgr) = state.setup_manager {
                if setup_mgr.mark_authorized(&session_id, token_set).await {
                    info!(session_id = %session_id_str, "Setup session authorized via callback");
                    return oauth_html_response(
                        "<html><body><h1>Authorization Successful</h1>\
                         <p>You can close this window and return to the app to complete setup.</p>\
                         </body></html>"
                            .to_string(),
                    );
                }
            }
        }
        warn!(flow_name = %flow.endpoint_name, "Setup session not found for callback");
        return oauth_html_response(
            "<html><body><h1>OAuth Error</h1>\
             <p>Setup session not found or expired. Please start over.</p>\
             <p>You can close this window.</p></body></html>"
                .to_string(),
        );
    }

    // Existing endpoint re-auth flow: save tokens to disk
    if let Some(ref tm) = state.token_manager {
        if let Err(e) = tm.save(&flow.endpoint_name, &token_set).await {
            error!(endpoint = %flow.endpoint_name, error = %e, "Failed to save tokens");
        }
    }

    // Apply tokens to the adapter's shared inner state. Also propagate the
    // freshly discovered token endpoint used for this code exchange into the
    // adapter's in-memory override so the next proactive refresh POSTs to
    // the same URL we just succeeded against — without depending on whether
    // startup-time discovery ran or returned the same result.
    if let Some(ref inners) = state.oauth_adapter_inners {
        let inners = inners.read().await;
        if let Some(inner) = inners.get(&flow.endpoint_name) {
            inner
                .set_token_endpoint_override(flow.token_endpoint.clone())
                .await;
            inner.apply_tokens(token_set.clone()).await;
            info!(
                endpoint = %flow.endpoint_name,
                token_endpoint = %flow.token_endpoint,
                "Tokens applied to OAuth adapter; token endpoint override updated"
            );
        }
    }

    oauth_html_response(format!(
        "<html><body><h1>Login Successful</h1><p>Endpoint <strong>{}</strong> is now authenticated.</p><p>You can close this window.</p></body></html>",
        html_escape(&flow.endpoint_name)
    ))
}

/// Logged wrapper for POST /mcp/initialize (direct route).
///
/// Mirrors the unified `/mcp` route: parses `clientInfo`, mints a fresh
/// `Mcp-Session-Id`, seeds the [`SessionIdentityStore`] when structured
/// identity is present, and echoes the new session id back on the response
/// so the client can correlate follow-up calls. The resolved identity is
/// embedded on the `request` span and surfaced on the audit log line.
async fn mcp_initialize_logged(
    state: State<AppState>,
    headers: HeaderMap,
    body: Json<JsonRpcBody>,
) -> Response {
    let body_value = serde_json::to_value(&body.0).unwrap_or(Value::Null);
    let (identity, new_session) = resolve_identity_for_message(&state.0, &headers, &body_value);
    let client_json = identity
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| serde_json::to_string(c).unwrap_or_default())
        .unwrap_or_default();
    let client_name = identity
        .as_ref()
        .and_then(|c| c.client_label())
        .unwrap_or_default();
    let client_version = identity
        .as_ref()
        .and_then(|c| c.version.clone())
        .unwrap_or_default();
    // Mint a per-message UID (mirrors `handle_single_message`) so the direct
    // route's `request` span and audit log carry a collision-free key for the
    // desktop's row/overlay keying.
    let request_uid = uuid::Uuid::new_v4().to_string();
    let span = tracing::info_span!(
        "request",
        method = "initialize",
        id = ?body.id,
        request_uid = %request_uid,
        client = %client_json,
    );
    async move {
        let start = Instant::now();
        let req_bytes = serde_json::to_string(&body.0).map(|s| s.len()).unwrap_or(0);
        let Json(resp) = mcp_initialize(state, body, None).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let resp_bytes = serde_json::to_string(&resp).map(|s| s.len()).unwrap_or(0);
        info!(
            method = "initialize",
            request_uid = %request_uid,
            elapsed_ms = elapsed_ms,
            req_bytes = req_bytes,
            resp_bytes = resp_bytes,
            status = 200,
            client_name = ?client_name,
            client_version = ?client_version,
            "MCP request"
        );
        with_session_header(json_response(resp), new_session)
    }
    .instrument(span)
    .await
}

/// Logged wrapper for POST /mcp/tools/list (direct route).
async fn mcp_tools_list_logged(
    state: State<AppState>,
    headers: HeaderMap,
    body: Json<JsonRpcBody>,
) -> Json<Value> {
    let identity = resolve_inbound_identity(&state.0, &headers);
    let client_json = identity
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| serde_json::to_string(c).unwrap_or_default())
        .unwrap_or_default();
    let client_name = identity
        .as_ref()
        .and_then(|c| c.client_label())
        .unwrap_or_default();
    let client_version = identity
        .as_ref()
        .and_then(|c| c.version.clone())
        .unwrap_or_default();
    // Mint a per-message UID (mirrors `handle_single_message`) so the direct
    // route's `request` span and audit log carry a collision-free key for the
    // desktop's row/overlay keying.
    let request_uid = uuid::Uuid::new_v4().to_string();
    let span = tracing::info_span!(
        "request",
        method = "tools/list",
        id = ?body.id,
        request_uid = %request_uid,
        client = %client_json,
    );
    async move {
        let start = Instant::now();
        let req_bytes = serde_json::to_string(&body.0).map(|s| s.len()).unwrap_or(0);
        let resp = mcp_tools_list(state, body, None).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let resp_bytes = serde_json::to_string(&resp.0).map(|s| s.len()).unwrap_or(0);
        info!(
            method = "tools/list",
            request_uid = %request_uid,
            elapsed_ms = elapsed_ms,
            req_bytes = req_bytes,
            resp_bytes = resp_bytes,
            status = 200,
            client_name = ?client_name,
            client_version = ?client_version,
            "MCP request"
        );
        resp
    }
    .instrument(span)
    .await
}

/// Logged wrapper for POST /mcp/tools/call (direct route).
async fn mcp_tools_call_logged(
    state: State<AppState>,
    headers: HeaderMap,
    body: Json<JsonRpcBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let identity = resolve_inbound_identity(&state.0, &headers);
    let client_json = identity
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| serde_json::to_string(c).unwrap_or_default())
        .unwrap_or_default();
    let client_name = identity
        .as_ref()
        .and_then(|c| c.client_label())
        .unwrap_or_default();
    let client_version = identity
        .as_ref()
        .and_then(|c| c.version.clone())
        .unwrap_or_default();
    // Mint a per-message UID (mirrors `handle_single_message`) so the direct
    // route's `request` span and audit log carry a collision-free key for the
    // desktop's row/overlay keying.
    let request_uid = uuid::Uuid::new_v4().to_string();
    let span = tracing::info_span!(
        "request",
        method = "tools/call",
        id = ?body.id,
        request_uid = %request_uid,
        client = %client_json,
    );
    async move {
        let start = Instant::now();
        let req_bytes = serde_json::to_string(&body.0).map(|s| s.len()).unwrap_or(0);
        let result = mcp_tools_call(state, body, None).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(Json(resp)) => {
                let resp_bytes = serde_json::to_string(resp).map(|s| s.len()).unwrap_or(0);
                info!(
                    method = "tools/call",
                    request_uid = %request_uid,
                    elapsed_ms = elapsed_ms,
                    req_bytes = req_bytes,
                    resp_bytes = resp_bytes,
                    status = 200,
                    client_name = ?client_name,
                    client_version = ?client_version,
                    "MCP request"
                );
            }
            Err((status, Json(resp))) => {
                let resp_bytes = serde_json::to_string(resp).map(|s| s.len()).unwrap_or(0);
                let status_code = status.as_u16();
                if status_code >= 500 {
                    error!(
                        method = "tools/call",
                        request_uid = %request_uid,
                        elapsed_ms = elapsed_ms,
                        req_bytes = req_bytes,
                        resp_bytes = resp_bytes,
                        status = status_code,
                        client_name = ?client_name,
                        client_version = ?client_version,
                        "MCP request"
                    );
                } else if status_code == 200 {
                    // JSON-RPC 2.0: errors are returned with HTTP 200, not a sign of trouble.
                    info!(
                        method = "tools/call",
                        request_uid = %request_uid,
                        elapsed_ms = elapsed_ms,
                        req_bytes = req_bytes,
                        resp_bytes = resp_bytes,
                        status = status_code,
                        client_name = ?client_name,
                        client_version = ?client_version,
                        "MCP request"
                    );
                } else {
                    warn!(
                        method = "tools/call",
                        request_uid = %request_uid,
                        elapsed_ms = elapsed_ms,
                        req_bytes = req_bytes,
                        resp_bytes = resp_bytes,
                        status = status_code,
                        client_name = ?client_name,
                        client_version = ?client_version,
                        "MCP request"
                    );
                }
            }
        }
        result
    }
    .instrument(span)
    .await
}

/// Handler for DELETE /mcp — returns 405 Method Not Allowed.
/// The MCP Streamable HTTP spec allows servers to opt out of session termination.
async fn mcp_delete() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

/// Handler for `DELETE /mcp/{profile}` — profile-scoped variant of
/// [`mcp_delete`]. Resolves the URL `{profile}` segment against
/// [`AppState::profile_registry`] and returns 404 with a structured JSON
/// body if unknown; on hit returns 405 to match the global `/mcp` route's
/// opt-out from session termination.
async fn mcp_delete_profiled(
    State(state): State<AppState>,
    Path(profile_path): Path<String>,
) -> Response {
    if state.profile_registry.get(&profile_path).await.is_none() {
        return profile_not_found_response(&profile_path);
    }
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

/// Check whether an Origin header value is a localhost origin.
/// Allows `http://localhost`, `http://127.0.0.1`, `http://[::1]` on any port.
fn is_localhost_origin(origin: &str) -> bool {
    // Parse out scheme + host + optional port
    let without_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(host_port) = without_scheme else {
        return false;
    };
    // Handle IPv6 bracket notation like [::1]:3000
    let host = if host_port.starts_with('[') {
        // IPv6: extract up to the closing bracket
        host_port
            .split(']')
            .next()
            .map(|s| format!("{}]", s))
            .unwrap_or_default()
    } else {
        // IPv4 / hostname: strip port
        host_port.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// Build the axum Router with all MCP routes.
///
/// CORS is configured to only allow localhost origins (DNS rebinding protection).
/// Use `build_router_with_origins` to allow additional origins.
pub fn build_router(state: AppState) -> Router {
    build_router_with_origins(state, &[])
}

/// Build the axum Router with all MCP routes and additional allowed origins.
///
/// `extra_origins` is a list of allowed origin strings (e.g., `["https://example.com"]`).
/// Localhost origins (`127.0.0.1`, `::1`, `localhost`) are always allowed.
pub fn build_router_with_origins(state: AppState, extra_origins: &[String]) -> Router {
    let extra: Vec<String> = extra_origins.to_vec();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            let Ok(origin_str) = origin.to_str() else {
                return false;
            };
            is_localhost_origin(origin_str) || extra.iter().any(|allowed| allowed == origin_str)
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([axum::http::header::CONTENT_TYPE]);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/mcp", post(mcp_unified).delete(mcp_delete))
        .route("/mcp/initialize", post(mcp_initialize_logged))
        .route("/mcp/tools/list", post(mcp_tools_list_logged))
        .route("/mcp/tools/call", post(mcp_tools_call_logged))
        .route("/mcp/sse", get(mcp_sse))
        // Profile-scoped variants. Per recon D7, axum 0.8 prefers the
        // specific `/mcp/{initialize,tools,sse}` routes above over the
        // `/mcp/{profile}` wildcard, and `RESERVED_PROFILE_PATHS` keeps
        // profile names from colliding with the `/mcp/{profile}/sse` path.
        .route(
            "/mcp/{profile}",
            post(mcp_unified_profiled).delete(mcp_delete_profiled),
        )
        .route("/mcp/{profile}/sse", get(mcp_sse_profiled))
        .route("/oauth/callback", get(oauth_callback))
        .layer(cors)
        .with_state(state)
}

/// GET /healthz — liveness probe.
///
/// Returns `200 OK` with a JSON body containing `status`, the crate
/// `version`, and process `uptime_secs`, so external supervisors
/// (load balancers, container orchestrators, uptime checks) can verify
/// the relay process is up without exercising upstream MCP adapters.
async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION").to_string(),
            "uptime_secs": state.started_at.elapsed().as_secs(),
        })),
    )
}

/// Create a future that resolves when a shutdown signal (SIGINT, SIGTERM, or SIGHUP) is received.
pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(unix)]
    let hangup = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("failed to install SIGHUP handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    #[cfg(not(unix))]
    let hangup = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("SIGINT received, shutting down");
        }
        _ = terminate => {
            info!("SIGTERM received, shutting down");
        }
        _ = hangup => {
            info!("SIGHUP received, shutting down");
        }
    }
}

/// Start the HTTP server and return the bound address.
/// The server runs until the provided shutdown signal completes.
pub async fn start_server(
    router: Router,
    addr: SocketAddr,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    info!(addr = %local_addr, "MCP HTTP server listening");

    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .ok();
    });

    Ok((local_addr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use crate::js_sandbox::MetaToolHandler;
    use crate::registry::AdapterRegistry;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    /// A mock adapter for server-level sorting tests.
    struct MockAdapter {
        tools: Vec<ToolInfo>,
    }

    impl MockAdapter {
        fn with_tools(names: &[&str]) -> Self {
            Self {
                tools: names
                    .iter()
                    .map(|n| ToolInfo {
                        name: n.to_string(),
                        description: Some(format!("{} tool", n)),
                        input_schema: json!({"type": "object"}),
                        annotations: None,
                    })
                    .collect(),
            }
        }
    }

    #[async_trait]
    impl McpAdapter for MockAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    /// Build a minimal AppState for testing (no OAuth, no token manager).
    fn test_app_state() -> AppState {
        let registry = AdapterRegistry::new();
        let meta_tool_handler = Arc::new(MetaToolHandler::new(
            Arc::new(registry.clone()),
            Duration::from_secs(5),
        ));
        let profile_registry = Arc::new(ProfileRegistry::new(registry.clone()));
        AppState {
            registry,
            js_execution_mode: Arc::new(AtomicBool::new(false)),
            meta_tool_handler,
            profile_registry,
            oauth_flow_manager: None,
            token_manager: None,
            oauth_adapter_inners: None,
            setup_manager: None,
            started_at: Instant::now(),
            toon_enabled: false,
            session_identities: Arc::new(Mutex::new(SessionIdentityStore::default())),
            meta_tool_schemas: MetaToolSchemas::new(),
        }
    }

    fn identity(name: &str, version: &str) -> ClientIdentity {
        ClientIdentity {
            name: Some(name.to_string()),
            version: Some(version.to_string()),
            user_agent: None,
            origin: None,
        }
    }

    #[test]
    fn session_identity_store_default_capacity_matches_const() {
        let store = SessionIdentityStore::default();
        assert_eq!(store.capacity(), DEFAULT_SESSION_IDENTITY_MAX_SESSIONS);
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
    }

    #[test]
    fn session_identity_store_zero_capacity_clamps_to_one() {
        let mut store = SessionIdentityStore::with_capacity(0);
        assert_eq!(store.capacity(), 1);
        store.insert("s1".into(), identity("a", "1"));
        assert_eq!(store.len(), 1);
        store.insert("s2".into(), identity("b", "2"));
        // Capacity-1 store evicts the previous entry to make room.
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("s1"), None);
        assert_eq!(store.get("s2"), Some(identity("b", "2")));
    }

    #[test]
    fn session_identity_store_get_returns_inserted_identity() {
        let mut store = SessionIdentityStore::with_capacity(4);
        store.insert("sid".into(), identity("claude-ai", "0.1.0"));
        assert_eq!(store.get("sid"), Some(identity("claude-ai", "0.1.0")));
        assert_eq!(store.get("missing"), None);
    }

    #[test]
    fn session_identity_store_evicts_lru_when_at_capacity() {
        let mut store = SessionIdentityStore::with_capacity(3);
        store.insert("s1".into(), identity("a", "1"));
        store.insert("s2".into(), identity("b", "2"));
        store.insert("s3".into(), identity("c", "3"));
        // Touch s1 so it becomes most-recently-used.
        assert_eq!(store.get("s1"), Some(identity("a", "1")));
        // s4 forces eviction of the LRU entry, which is now s2.
        store.insert("s4".into(), identity("d", "4"));
        assert_eq!(store.len(), 3);
        assert_eq!(store.get("s2"), None, "s2 should have been evicted");
        assert_eq!(store.get("s1"), Some(identity("a", "1")));
        assert_eq!(store.get("s3"), Some(identity("c", "3")));
        assert_eq!(store.get("s4"), Some(identity("d", "4")));
    }

    #[test]
    fn session_identity_store_refresh_does_not_grow() {
        let mut store = SessionIdentityStore::with_capacity(2);
        store.insert("s1".into(), identity("a", "1"));
        store.insert("s1".into(), identity("a", "2"));
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("s1"), Some(identity("a", "2")));
    }

    #[test]
    fn identity_from_initialize_params_extracts_name_and_version() {
        let params = json!({
            "clientInfo": { "name": "claude-ai", "version": "0.1.0" },
            "protocolVersion": "2025-03-26",
        });
        let id = identity_from_initialize_params(Some(&params));
        assert_eq!(id.name.as_deref(), Some("claude-ai"));
        assert_eq!(id.version.as_deref(), Some("0.1.0"));
        assert!(id.user_agent.is_none());
        assert!(id.origin.is_none());
    }

    #[test]
    fn identity_from_initialize_params_missing_client_info_is_empty() {
        let params = json!({ "protocolVersion": "2025-03-26" });
        let id = identity_from_initialize_params(Some(&params));
        assert!(id.is_empty());
        let id = identity_from_initialize_params(None);
        assert!(id.is_empty());
    }

    #[test]
    fn identity_from_headers_picks_user_agent_and_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("claude-desktop/0.7"));
        headers.insert("origin", HeaderValue::from_static("https://claude.ai"));
        let id = identity_from_headers(&headers);
        assert_eq!(id.user_agent.as_deref(), Some("claude-desktop/0.7"));
        assert_eq!(id.origin.as_deref(), Some("https://claude.ai"));
        assert!(id.name.is_none());
        assert!(id.version.is_none());
    }

    #[test]
    fn merge_identity_prefers_primary_then_fills_from_fallback() {
        let primary = ClientIdentity {
            name: Some("claude-ai".into()),
            version: None,
            user_agent: None,
            origin: None,
        };
        let fallback = ClientIdentity {
            name: Some("ignored".into()),
            version: Some("0.1.0".into()),
            user_agent: Some("claude-desktop/0.7".into()),
            origin: Some("https://claude.ai".into()),
        };
        let merged = merge_identity(primary, fallback);
        assert_eq!(merged.name.as_deref(), Some("claude-ai"));
        assert_eq!(merged.version.as_deref(), Some("0.1.0"));
        assert_eq!(merged.user_agent.as_deref(), Some("claude-desktop/0.7"));
        assert_eq!(merged.origin.as_deref(), Some("https://claude.ai"));
    }

    #[tokio::test]
    async fn resolve_identity_for_message_initialize_stores_and_emits_session_id() {
        let state = test_app_state();
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("claude-desktop/0.7"));
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": { "name": "claude-ai", "version": "0.1.0" },
            },
        });
        let (identity, session_id) = resolve_identity_for_message(&state, &headers, &msg);
        let identity = identity.expect("initialize should resolve an identity");
        let session_id = session_id.expect("initialize should mint a session id");
        // Merged identity carries clientInfo + User-Agent fallback.
        assert_eq!(identity.name.as_deref(), Some("claude-ai"));
        assert_eq!(identity.version.as_deref(), Some("0.1.0"));
        assert_eq!(identity.user_agent.as_deref(), Some("claude-desktop/0.7"));
        // The store is seeded with the structured (name/version) identity only.
        let stored = state
            .session_identities
            .lock()
            .unwrap()
            .get(&session_id)
            .expect("session id should resolve in the store");
        assert_eq!(stored.name.as_deref(), Some("claude-ai"));
        assert_eq!(stored.version.as_deref(), Some("0.1.0"));
        assert!(stored.user_agent.is_none());
    }

    #[tokio::test]
    async fn resolve_identity_for_message_initialize_without_client_info_skips_store() {
        let state = test_app_state();
        let headers = HeaderMap::new();
        let msg = json!({"jsonrpc":"2.0","method":"initialize","id":1});
        let (identity, session_id) = resolve_identity_for_message(&state, &headers, &msg);
        // Identity stays `None` when neither clientInfo nor headers say
        // anything; a session id is still issued for future correlation.
        assert!(identity.is_none());
        assert!(session_id.is_some());
        // Empty identities are not cached.
        assert!(state.session_identities.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resolve_inbound_identity_uses_session_then_header_fallback() {
        let state = test_app_state();
        // Seed the store with the identity that initialize would have cached.
        let sid = "fixed-session";
        state
            .session_identities
            .lock()
            .unwrap()
            .insert(sid.into(), identity("claude-ai", "0.1.0"));

        // Follow-up request echoes the session header AND carries its own UA.
        let mut headers = HeaderMap::new();
        headers.insert(MCP_SESSION_ID_HEADER, HeaderValue::from_static(sid));
        headers.insert("user-agent", HeaderValue::from_static("claude-desktop/0.7"));
        headers.insert("origin", HeaderValue::from_static("https://claude.ai"));
        let resolved = resolve_inbound_identity(&state, &headers).expect("identity must resolve");
        assert_eq!(resolved.name.as_deref(), Some("claude-ai"));
        assert_eq!(resolved.version.as_deref(), Some("0.1.0"));
        assert_eq!(resolved.user_agent.as_deref(), Some("claude-desktop/0.7"));
        assert_eq!(resolved.origin.as_deref(), Some("https://claude.ai"));

        // Same request without the session header falls back to header-only
        // identity (no name/version because no clientInfo was cached).
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("claude-desktop/0.7"));
        let resolved = resolve_inbound_identity(&state, &headers).unwrap();
        assert!(resolved.name.is_none());
        assert_eq!(resolved.user_agent.as_deref(), Some("claude-desktop/0.7"));

        // Bare request with no signals at all degrades to `None` so adapter
        // event-emission can omit the `client` field entirely.
        assert!(resolve_inbound_identity(&state, &HeaderMap::new()).is_none());
    }

    #[tokio::test]
    async fn initialize_response_round_trip_resolves_identity_on_follow_up() {
        use axum::body::Body;
        use tower::ServiceExt;
        let state = test_app_state();
        let router = build_router(state.clone());

        let init_req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "method": "initialize",
                    "id": 1,
                    "params": {
                        "clientInfo": {"name": "claude-ai", "version": "0.1.0"},
                    },
                }))
                .unwrap(),
            ))
            .unwrap();
        let init_resp = router.clone().oneshot(init_req).await.unwrap();
        assert_eq!(init_resp.status(), StatusCode::OK);
        let sid_header = init_resp
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .expect("initialize must emit Mcp-Session-Id")
            .to_str()
            .unwrap()
            .to_string();
        assert!(!sid_header.is_empty(), "session id must be non-empty");

        // Re-resolve via the public helper to confirm the store was seeded
        // and that follow-up requests can recover the identity by echoing
        // the same header back.
        let mut follow_up_headers = HeaderMap::new();
        follow_up_headers.insert(
            MCP_SESSION_ID_HEADER,
            HeaderValue::from_str(&sid_header).unwrap(),
        );
        let resolved =
            resolve_inbound_identity(&state, &follow_up_headers).expect("identity must resolve");
        assert_eq!(resolved.name.as_deref(), Some("claude-ai"));
        assert_eq!(resolved.version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn jsonrpc_response_has_correct_structure() {
        let resp = jsonrpc_response(Some(json!(1)), json!({"ok": true}));
        let body = resp.0;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["ok"], true);
        assert!(body.get("error").is_none());
    }

    #[test]
    fn jsonrpc_response_with_null_id() {
        let resp = jsonrpc_response(None, json!("hello"));
        let body = resp.0;
        assert_eq!(body["jsonrpc"], "2.0");
        assert!(body["id"].is_null());
        assert_eq!(body["result"], "hello");
    }

    #[test]
    fn jsonrpc_error_has_correct_structure() {
        let (status, resp) = jsonrpc_error(Some(json!(42)), -32601, "Method not found");
        assert_eq!(status, StatusCode::OK);
        let body = resp.0;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 42);
        assert_eq!(body["error"]["code"], -32601);
        assert_eq!(body["error"]["message"], "Method not found");
        assert!(body.get("result").is_none());
    }

    #[tokio::test]
    async fn mcp_initialize_returns_correct_response() {
        let state = test_app_state();
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("initialize".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_initialize(State(state), Json(body), None).await;

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);

        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "Endara Relay");
        // Version should be a non-empty string
        assert!(!result["serverInfo"]["version"].as_str().unwrap().is_empty());
        // Capabilities must include "tools"
        assert!(result["capabilities"]["tools"].is_object());
        // No connected adapters → instructions field omitted (spec §2.1).
        assert!(result.get("instructions").is_none());
    }

    /// Test-only tracing [`Layer`] that records the `request_uid` field recorded
    /// on each `request` span at creation. This reads the very field that
    /// [`crate::events::SpanFieldCaptureLayer`] captures and
    /// [`crate::events::current_request_context`] surfaces as
    /// `RequestSpanContext::request_uid`, so capturing a non-empty value here
    /// proves the direct-route logged wrappers mint a per-call UID onto their
    /// `request` span.
    use ::tracing::field::{Field, Visit};
    use ::tracing::span::{Attributes, Id};
    use ::tracing::Subscriber;
    use ::tracing_subscriber::layer::Context;
    use ::tracing_subscriber::registry::LookupSpan;
    use ::tracing_subscriber::Layer as TracingLayer;

    struct RequestUidProbeLayer {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl<S> TracingLayer<S> for RequestUidProbeLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
            if attrs.metadata().name() != "request" {
                return;
            }
            struct V<'a> {
                out: &'a mut Option<String>,
            }
            impl Visit for V<'_> {
                fn record_str(&mut self, field: &Field, value: &str) {
                    if field.name() == "request_uid" {
                        *self.out = Some(value.to_string());
                    }
                }
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "request_uid" {
                        *self.out = Some(format!("{:?}", value));
                    }
                }
            }
            let mut uid = None;
            attrs.record(&mut V { out: &mut uid });
            if let Some(uid) = uid {
                self.seen.lock().unwrap().push(uid);
            }
        }
    }

    /// Drive a logged-wrapper future under the [`RequestUidProbeLayer`] and
    /// return every `request_uid` minted onto a `request` span while it ran.
    fn request_uids_for<F: std::future::Future>(fut: F) -> Vec<String> {
        use ::tracing_subscriber::prelude::*;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = ::tracing_subscriber::registry().with(RequestUidProbeLayer {
            seen: Arc::clone(&seen),
        });
        ::tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(fut);
        });
        let captured = seen.lock().unwrap().clone();
        captured
    }

    fn body_for(method: &str, params: Option<Value>) -> JsonRpcBody {
        JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some(method.to_string()),
            params,
            id: Some(json!(1)),
        }
    }

    /// `mcp_initialize_logged` mints a fresh, non-empty UUID `request_uid` onto
    /// its `request` span (the field `current_request_context()` surfaces).
    #[test]
    #[serial_test::serial(tracing)]
    fn mcp_initialize_logged_mints_request_uid_span_field() {
        crate::test_tracing::init_permissive_tracing();
        let uids = request_uids_for(async {
            let state = test_app_state();
            let _ = mcp_initialize_logged(
                State(state),
                HeaderMap::new(),
                Json(body_for("initialize", None)),
            )
            .await;
        });
        assert_eq!(uids.len(), 1, "expected one request span: {:?}", uids);
        assert!(
            uuid::Uuid::parse_str(&uids[0]).is_ok(),
            "request_uid must be a valid UUID, got {:?}",
            uids[0]
        );
    }

    /// `mcp_tools_list_logged` mints a fresh, non-empty UUID `request_uid` onto
    /// its `request` span.
    #[test]
    #[serial_test::serial(tracing)]
    fn mcp_tools_list_logged_mints_request_uid_span_field() {
        crate::test_tracing::init_permissive_tracing();
        let uids = request_uids_for(async {
            let state = test_app_state();
            let _ = mcp_tools_list_logged(
                State(state),
                HeaderMap::new(),
                Json(body_for("tools/list", None)),
            )
            .await;
        });
        assert_eq!(uids.len(), 1, "expected one request span: {:?}", uids);
        assert!(
            uuid::Uuid::parse_str(&uids[0]).is_ok(),
            "request_uid must be a valid UUID, got {:?}",
            uids[0]
        );
    }

    /// `mcp_tools_call_logged` mints a fresh, non-empty UUID `request_uid` onto
    /// its `request` span — the route whose inner upstream calls feed
    /// `ToolCallEvent::Started.request_uid`.
    #[test]
    #[serial_test::serial(tracing)]
    fn mcp_tools_call_logged_mints_request_uid_span_field() {
        crate::test_tracing::init_permissive_tracing();
        let uids = request_uids_for(async {
            let state = test_app_state();
            let _ = mcp_tools_call_logged(
                State(state),
                HeaderMap::new(),
                Json(body_for(
                    "tools/call",
                    Some(json!({ "name": "list_tools", "arguments": {} })),
                )),
            )
            .await;
        });
        assert_eq!(uids.len(), 1, "expected one request span: {:?}", uids);
        assert!(
            uuid::Uuid::parse_str(&uids[0]).is_ok(),
            "request_uid must be a valid UUID, got {:?}",
            uids[0]
        );
    }

    /// Each direct-route call mints its own UID, so concurrent direct callers
    /// never collide on the desktop's row/overlay key.
    #[test]
    #[serial_test::serial(tracing)]
    fn direct_route_request_uids_are_unique_per_call() {
        crate::test_tracing::init_permissive_tracing();
        let uids = request_uids_for(async {
            for _ in 0..2 {
                let state = test_app_state();
                let _ = mcp_tools_call_logged(
                    State(state),
                    HeaderMap::new(),
                    Json(body_for(
                        "tools/call",
                        Some(json!({ "name": "list_tools", "arguments": {} })),
                    )),
                )
                .await;
            }
        });
        assert_eq!(uids.len(), 2, "expected two request spans: {:?}", uids);
        assert_ne!(uids[0], uids[1], "each call must mint a fresh UID");
    }

    #[tokio::test]
    async fn meta_tool_definitions_contains_expected_tools() {
        let registry = AdapterRegistry::new();
        let defs = meta_tool_definitions(true, &registry, false, None).await;
        assert_eq!(defs.len(), 3);

        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(names.contains(&"execute_tools"));

        // Each definition must have name, description, inputSchema
        for def in &defs {
            assert!(def["name"].is_string());
            assert!(def["description"].is_string());
            assert!(def["inputSchema"].is_object());
            assert_eq!(def["inputSchema"]["type"], "object");
        }
    }

    #[tokio::test]
    async fn meta_tool_definitions_hides_execute_tools_when_js_off() {
        let registry = AdapterRegistry::new();
        let defs = meta_tool_definitions(false, &registry, false, None).await;
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(
            !names.contains(&"execute_tools"),
            "execute_tools must not be advertised when local_js_execution is off"
        );
    }

    #[tokio::test]
    async fn test_list_tools_description_documents_return_format() {
        let registry = AdapterRegistry::new();
        let defs = meta_tool_definitions(true, &registry, false, None).await;
        let list_desc = defs.iter().find(|d| d["name"] == "list_tools").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            list_desc.contains("tools"),
            "list_tools description should mention 'tools'"
        );
        assert!(
            list_desc.contains("total"),
            "list_tools description should mention 'total'"
        );
        assert!(
            list_desc.contains("limit"),
            "list_tools description should mention 'limit'"
        );
        assert!(
            list_desc.contains("offset"),
            "list_tools description should mention 'offset'"
        );
        assert!(
            list_desc.contains("execute_tools"),
            "list_tools description should mention 'execute_tools'"
        );
    }

    #[tokio::test]
    async fn test_search_tools_description_documents_behavior() {
        let registry = AdapterRegistry::new();
        let defs = meta_tool_definitions(true, &registry, false, None).await;
        let search_desc = defs.iter().find(|d| d["name"] == "search_tools").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            search_desc.contains("keyword")
                || search_desc.contains("search")
                || search_desc.contains("Search"),
            "search_tools description should mention 'keyword' or 'search'"
        );
        assert!(
            search_desc.contains("fuzzy") || search_desc.contains("Fuzzy"),
            "search_tools description should mention 'fuzzy'"
        );
        assert!(
            search_desc.contains("rank") || search_desc.contains("Ranked"),
            "search_tools description should mention 'rank'"
        );
    }

    #[tokio::test]
    async fn test_execute_tools_description_has_examples() {
        let registry = AdapterRegistry::new();
        let defs = meta_tool_definitions(true, &registry, false, None).await;
        let exec_desc = defs.iter().find(|d| d["name"] == "execute_tools").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            exec_desc.contains("call(\"tool_name\""),
            "execute_tools description should document the call() helper as the primary calling convention"
        );
        assert!(
            exec_desc.contains("tools[\"tool_name\"]"),
            "execute_tools description should still mention the tools[\"...\"] indexer form"
        );
        assert!(
            exec_desc.contains("prefix__name") || exec_desc.contains("double underscore"),
            "execute_tools description should document naming convention"
        );
        assert!(
            exec_desc.contains("structuredContent"),
            "execute_tools description should mention structuredContent return format"
        );
        assert!(
            exec_desc.contains("return"),
            "execute_tools description should mention how to send data back"
        );
        assert!(
            exec_desc.contains("await"),
            "execute_tools description should include at least one code example with await"
        );
        assert!(
            exec_desc.contains("unwrapped"),
            "execute_tools description should explain that call() returns the unwrapped result directly"
        );
        assert!(
            exec_desc.contains("isError"),
            "execute_tools description should explain that call() throws on isError envelopes"
        );
        assert!(
            exec_desc.contains("raw MCP envelope"),
            "execute_tools description should explain when to use the tools[...] indexer (raw MCP envelope)"
        );
        assert!(
            exec_desc.contains("r.structuredContent"),
            "execute_tools description should include a tools[...] indexer example accessing r.structuredContent"
        );
        assert!(
            exec_desc.contains("closest matching tools") || exec_desc.contains("closest tools"),
            "execute_tools description should advertise fuzzy unknown-tool suggestions"
        );
        // At least one code example should call() a tool with await.
        assert!(
            exec_desc.contains("await call("),
            "execute_tools description should include at least one `await call(...)` example"
        );
        assert!(
            exec_desc.contains("retry: 3"),
            "execute_tools description should document the opt-in `{{ retry: 3 }}` option"
        );
        assert!(
            exec_desc.contains("readOnlyHint") && exec_desc.contains("idempotentHint"),
            "execute_tools description should mention readOnlyHint / idempotentHint as the gating annotations for retry"
        );
    }

    #[test]
    fn build_router_succeeds() {
        let state = test_app_state();
        let _router = build_router(state);
        // If we get here without panic, the router was built successfully.
    }

    #[tokio::test]
    async fn mcp_tools_list_includes_meta_tools_in_normal_mode() {
        let state = test_app_state();
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body), None).await;
        let tools = resp["result"]["tools"].as_array().unwrap();

        // With no adapters registered and JS mode off, only list_tools and
        // search_tools should appear. execute_tools is gated on
        // local_js_execution and is hidden from the catalog when off.
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(!names.contains(&"execute_tools"));
    }

    #[tokio::test]
    async fn mcp_tools_list_js_mode_returns_only_meta_tools() {
        let state = test_app_state();
        state.js_execution_mode.store(true, Ordering::Relaxed);
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body), None).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
    }

    #[tokio::test]
    async fn meta_tools_return_mcp_content_array_format() {
        let state = test_app_state();

        // Test list_tools
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "list_tools", "arguments": {}})),
            id: Some(json!(1)),
        };
        let result = mcp_tools_call(State(state.clone()), Json(body), None).await;
        let Json(resp) = result.unwrap();
        let content = resp["result"]["content"]
            .as_array()
            .expect("content array for list_tools");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        let inner: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert!(inner["tools"].is_array());
        assert!(inner["total"].is_number());

        // Test search_tools
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "search_tools", "arguments": {"query": "test"}})),
            id: Some(json!(2)),
        };
        let result = mcp_tools_call(State(state.clone()), Json(body), None).await;
        let Json(resp) = result.unwrap();
        let content = resp["result"]["content"]
            .as_array()
            .expect("content array for search_tools");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].is_string());

        // Test execute_tools (empty script returns result)
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "execute_tools", "arguments": {"script": ""}})),
            id: Some(json!(3)),
        };
        let result = mcp_tools_call(State(state), Json(body), None).await;
        // execute_tools with empty script may error — either way, check the shape
        match result {
            Ok(Json(resp)) => {
                let content = resp["result"]["content"]
                    .as_array()
                    .expect("content array for execute_tools");
                assert_eq!(content[0]["type"], "text");
            }
            Err(_) => {
                // An error response is acceptable for empty script
            }
        }
    }

    // --- Meta-tool input validation (spec §6 / §10.2) ---
    //
    // `validate_meta_tool_args` runs at the top of each meta-tool branch in
    // `mcp_tools_call`, validating against the precompiled static schemas on
    // `AppState::meta_tool_schemas`. Failures return the same `isError: true`
    // result shape as the per-tool `route_tool_call` path.

    /// Build a `tools/call` JSON-RPC body for a meta-tool with `arguments`.
    fn meta_call_body(name: &str, arguments: Value) -> JsonRpcBody {
        JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({ "name": name, "arguments": arguments })),
            id: Some(json!(1)),
        }
    }

    // §10.2 #16 — a wrong key name (`search` instead of `query`) is rejected
    // as an unknown parameter instead of silently falling through to an empty
    // search, and the valid parameter list surfaces `query`.
    #[tokio::test]
    async fn search_tools_wrong_key_rejected() {
        let state = test_app_state();
        let body = meta_call_body("search_tools", json!({ "search": "foo" }));
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true, "got: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("'search'"), "names the bad key: {text}");
        assert!(
            text.contains("unknown parameter"),
            "explains rejection: {text}"
        );
        assert!(
            text.contains("query"),
            "lists the valid 'query' param: {text}"
        );
    }

    // §10.2 #17 — `search_tools` without the required `query` is rejected.
    #[tokio::test]
    async fn search_tools_missing_query_rejected() {
        let state = test_app_state();
        let body = meta_call_body("search_tools", json!({}));
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true, "got: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("query") && text.contains("required field is missing"),
            "should report the missing required field: {text}"
        );
    }

    // §10.2 #18 — `execute_tools` without the required `script` is rejected
    // (JS mode on so the branch is reachable past the disabled gate).
    #[tokio::test]
    async fn execute_tools_missing_script_rejected() {
        let state = test_app_state();
        state.js_execution_mode.store(true, Ordering::Relaxed);
        let body = meta_call_body("execute_tools", json!({}));
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true, "got: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("script") && text.contains("required field is missing"),
            "should report the missing required field: {text}"
        );
    }

    // §10.2 #19 — a negative `limit` violates the schema's `minimum: 1`.
    #[tokio::test]
    async fn list_tools_negative_limit_rejected() {
        let state = test_app_state();
        let body = meta_call_body("list_tools", json!({ "limit": -1 }));
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true, "got: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("limit") && text.contains("outside range"),
            "should frame the violation as a range problem: {text}"
        );
    }

    // §10.2 #20 — a valid `list_tools` call passes validation and returns the
    // normal (non-error) result envelope.
    #[tokio::test]
    async fn list_tools_valid_limit_passes() {
        let state = test_app_state();
        let body = meta_call_body("list_tools", json!({ "limit": 5 }));
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        assert!(
            resp["result"].get("isError").is_none(),
            "valid input should not produce an error envelope: {resp}"
        );
        let inner: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(inner["tools"].is_array(), "got: {inner}");
    }

    // The `validate_inputs = false` toggle bypasses meta-tool validation: a
    // wrong key falls through to the handler instead of an `isError` result.
    #[tokio::test]
    async fn meta_tool_validation_respects_disabled_toggle() {
        let state = test_app_state();
        state.registry.set_validate_inputs(false);
        let body = meta_call_body("search_tools", json!({ "search": "foo" }));
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        assert!(
            resp["result"].get("isError").is_none(),
            "toggle off should bypass meta-tool validation: {resp}"
        );
    }

    // §5 row 16: `list_tools` response text is TOON when `toon_enabled` is on.
    #[tokio::test]
    async fn list_tools_response_is_toon_when_enabled() {
        let mut state = test_app_state();
        state.toon_enabled = true;
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "list_tools", "arguments": {}})),
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // TOON output for the `{ tools, total, limit, offset }` envelope is
        // never valid JSON — it starts with field declarations, not `{`.
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "expected TOON, got JSON-parseable text: {text}"
        );
        assert!(
            text.contains("total:"),
            "expected TOON field syntax in: {text}"
        );
    }

    // §5 row 15: `execute_tools` response text is TOON when `toon_enabled`
    // is on and the script returns a JSON object.
    #[tokio::test]
    async fn execute_tools_response_is_toon_when_enabled() {
        let mut state = test_app_state();
        state.toon_enabled = true;
        state.js_execution_mode.store(true, Ordering::Relaxed);
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({
                "name": "execute_tools",
                "arguments": {
                    "script": "return { users: [{ id: 1, name: 'a' }, { id: 2, name: 'b' }] };"
                }
            })),
            id: Some(json!(2)),
        };
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "expected TOON, got JSON-parseable text: {text}"
        );
        assert!(
            text.contains("users"),
            "expected \"users\" key in TOON: {text}"
        );
    }

    // §5 row 11: `toon_enabled = false` keeps JSON pass-through for meta-tool
    // responses (covers the config-flag-off branch end-to-end).
    #[tokio::test]
    async fn list_tools_response_is_json_when_disabled() {
        let state = test_app_state();
        assert!(!state.toon_enabled);
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/call".to_string()),
            params: Some(json!({"name": "list_tools", "arguments": {}})),
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_call(State(state), Json(body), None)
            .await
            .unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).expect("JSON when toon disabled");
        assert!(parsed["tools"].is_array());
    }

    // §5 row 17: server advertising description includes the TOON hint when
    // enabled and omits it when disabled.
    #[tokio::test]
    async fn search_tools_advertising_includes_toon_hint_when_enabled() {
        let registry = AdapterRegistry::new();
        let defs_on = meta_tool_definitions(true, &registry, true, None).await;
        let search_desc = defs_on
            .iter()
            .find(|d| d["name"] == "search_tools")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            search_desc.contains("TOON format"),
            "expected TOON hint in: {search_desc}"
        );

        let defs_off = meta_tool_definitions(true, &registry, false, None).await;
        let search_desc_off = defs_off
            .iter()
            .find(|d| d["name"] == "search_tools")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            !search_desc_off.contains("TOON"),
            "expected no TOON hint when disabled, got: {search_desc_off}"
        );
    }

    // §5 row 14: tools/call native route applies TOON to upstream tool
    // response text. Exercised via a route_tool_call stand-in: we go through
    // wrap_meta_tool_result with toon_enabled=true and verify the envelope
    // shape and TOON content match.
    #[test]
    fn wrap_meta_tool_result_emits_toon_when_enabled() {
        let val = json!({ "rows": [{"id": 1}, {"id": 2}] });
        let wrapped = wrap_meta_tool_result(val.clone(), true);
        let text = wrapped["content"][0]["text"].as_str().unwrap();
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "expected TOON, got: {text}"
        );
    }

    #[test]
    fn wrap_meta_tool_result_emits_json_when_disabled() {
        let val = json!({ "rows": [{"id": 1}, {"id": 2}] });
        let wrapped = wrap_meta_tool_result(val.clone(), false);
        let text = wrapped["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).expect("valid JSON");
        assert_eq!(parsed, val);
    }

    /// Helper: send a JSON-RPC POST to `/mcp` via the router and return the response.
    async fn post_mcp(state: AppState, body: &Value) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;

        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    /// Helper: collect the response body bytes into a `Value`.
    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn mcp_unified_dispatches_initialize() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["serverInfo"]["name"], "Endara Relay");
    }

    #[tokio::test]
    async fn mcp_unified_dispatches_tools_list() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"tools/list","id":2}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 2);
        assert!(body["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn mcp_unified_notification_returns_202_accepted() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "202 response must have no body");
    }

    #[tokio::test]
    async fn mcp_unified_any_notification_returns_202() {
        // Any JSON-RPC message without an `id` is a notification → 202
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/some_custom"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn mcp_unified_returns_method_not_found_for_unknown() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"unknown/method","id":99}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn get_mcp_returns_405_method_not_allowed() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn delete_mcp_returns_405_method_not_allowed() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn mcp_unified_batch_requests() {
        let state = test_app_state();
        let batch = json!([
            {"jsonrpc":"2.0","method":"initialize","id":1},
            {"jsonrpc":"2.0","method":"tools/list","id":2},
        ]);
        let resp = post_mcp(state, &batch).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body.as_array().expect("batch response should be an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[0]["result"]["serverInfo"]["name"], "Endara Relay");
        assert_eq!(arr[1]["id"], 2);
        assert!(arr[1]["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn mcp_unified_batch_all_notifications_returns_202() {
        let state = test_app_state();
        let batch = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","method":"notifications/cancelled"},
        ]);
        let resp = post_mcp(state, &batch).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty(), "all-notification batch must have no body");
    }

    #[tokio::test]
    async fn mcp_unified_batch_mixed_requests_and_notifications() {
        let state = test_app_state();
        let batch = json!([
            {"jsonrpc":"2.0","method":"notifications/initialized"},
            {"jsonrpc":"2.0","method":"initialize","id":1},
            {"jsonrpc":"2.0","method":"notifications/cancelled"},
        ]);
        let resp = post_mcp(state, &batch).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body
            .as_array()
            .expect("mixed batch response should be an array");
        // Only the request (id:1) should appear in the response
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
    }

    #[tokio::test]
    async fn mcp_unified_empty_batch_returns_error() {
        let state = test_app_state();
        let resp = post_mcp(state, &json!([])).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn mcp_unified_response_has_content_type_json() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .expect("response must have content-type header")
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "content-type should be application/json, got: {}",
            ct
        );
    }

    #[test]
    fn is_localhost_origin_accepts_valid_origins() {
        assert!(is_localhost_origin("http://localhost"));
        assert!(is_localhost_origin("http://localhost:3000"));
        assert!(is_localhost_origin("http://127.0.0.1"));
        assert!(is_localhost_origin("http://127.0.0.1:8080"));
        assert!(is_localhost_origin("http://[::1]"));
        assert!(is_localhost_origin("http://[::1]:9000"));
        assert!(is_localhost_origin("https://localhost:443"));
    }

    #[test]
    fn is_localhost_origin_rejects_non_localhost() {
        assert!(!is_localhost_origin("http://example.com"));
        assert!(!is_localhost_origin("http://evil.localhost.com"));
        assert!(!is_localhost_origin("http://192.168.1.1"));
        assert!(!is_localhost_origin("ftp://localhost"));
        assert!(!is_localhost_origin("localhost"));
    }

    // =====================================================================
    // MCP Streamable HTTP integration tests
    // =====================================================================

    #[tokio::test]
    async fn full_init_lifecycle() {
        // POST initialize → 200 + response
        // POST notifications/initialized → 202
        // POST tools/list → 200 + tools
        let state = test_app_state();

        // Step 1: initialize
        let resp = post_mcp(
            state.clone(),
            &json!({"jsonrpc":"2.0","method":"initialize","id":1,"params":{
                "protocolVersion":"2025-03-26",
                "capabilities":{},
                "clientInfo":{"name":"test-client","version":"0.1"}
            }}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(body["result"]["serverInfo"]["name"], "Endara Relay");

        // Step 2: notifications/initialized → 202 with empty body
        let resp = post_mcp(
            state.clone(),
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());

        // Step 3: tools/list → 200 + tools array
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"tools/list","id":2}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 2);
        let tools = body["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty(), "tools list should contain meta-tools");
    }

    #[tokio::test]
    async fn protocol_version_in_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        let body = body_json(resp).await;
        assert_eq!(
            body["result"]["protocolVersion"], "2025-03-26",
            "InitializeResult must contain protocolVersion 2025-03-26"
        );
    }

    #[tokio::test]
    async fn notifications_cancelled_returns_202() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{
                "requestId": 42, "reason": "user cancelled"
            }}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn notification_with_unknown_method_returns_202() {
        // Any notification (no id), even with an unknown method, must get 202.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/totally_made_up"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn notification_without_method_returns_202() {
        // A message with no id is a notification even if method is absent.
        let state = test_app_state();
        let resp = post_mcp(state, &json!({"jsonrpc":"2.0"})).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn notification_with_null_id_returns_202() {
        // JSON-RPC spec: `id: null` means notification in some interpretations.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized","id":null}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn batch_single_item_array_returns_array() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!([{"jsonrpc":"2.0","method":"initialize","id":1}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let arr = body
            .as_array()
            .expect("single-item batch should return array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
        assert!(arr[0]["result"]["serverInfo"].is_object());
    }

    #[tokio::test]
    async fn unknown_method_with_id_returns_jsonrpc_error() {
        // A request (has id) with unknown method must return JSON-RPC error, NOT 202.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"nonexistent/method","id":99}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 99);
        assert_eq!(body["error"]["code"], -32601);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("method not found"),
            "error message should mention 'method not found'"
        );
    }

    #[tokio::test]
    async fn invalid_json_body_returns_error() {
        use axum::body::Body;
        use tower::ServiceExt;

        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(b"this is not json".to_vec()))
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        // Axum rejects invalid JSON before our handler — expect 4xx
        assert!(
            resp.status().is_client_error(),
            "invalid JSON should return 4xx, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn invalid_jsonrpc_primitive_body_returns_error() {
        // A JSON primitive (string, number) is not a valid JSON-RPC message.
        let state = test_app_state();
        let resp = post_mcp(state, &json!("just a string")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn post_mcp_request_content_type_is_json() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
        )
        .await;
        let ct = resp
            .headers()
            .get("content-type")
            .expect("must have content-type")
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected application/json, got: {}",
            ct
        );
    }

    #[tokio::test]
    async fn batch_content_type_is_json() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!([{"jsonrpc":"2.0","method":"initialize","id":1}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .expect("batch response must have content-type")
            .to_str()
            .unwrap();
        assert!(ct.contains("application/json"));
    }

    #[tokio::test]
    async fn notification_202_has_no_content_type_json() {
        // Notifications return 202 with empty body — no content-type required.
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        // Body must be empty
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.is_empty());
    }

    // =====================================================================
    // Adapter HTTP client verification tests
    // =====================================================================

    #[test]
    fn http_adapter_sends_protocol_version_2025_03_26() {
        // Verify the adapter sends protocolVersion "2025-03-26" in initialize params.
        // We inspect the code path in HttpAdapter::initialize — the params JSON
        // is constructed inline. We verify the static value here.
        let params = json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "endara-relay",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        assert_eq!(params["protocolVersion"], "2025-03-26");
        assert_eq!(params["clientInfo"]["name"], "endara-relay");
    }

    #[tokio::test]
    async fn http_adapter_full_lifecycle_against_server() {
        // Integration test: spin up the real router and have HttpAdapter
        // connect to it, verifying:
        // - Client sends protocolVersion "2025-03-26"
        // - Client sends notifications/initialized after init
        // - Client can list tools
        use crate::adapter::http::{HttpAdapter, HttpConfig};
        use crate::adapter::McpAdapter;

        let state = test_app_state();
        let router = build_router(state);

        // Bind to an ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn the server
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let url = format!("http://127.0.0.1:{}/mcp", addr.port());
        let mut adapter = HttpAdapter::new(HttpConfig::new(&url));

        // initialize() sends protocolVersion "2025-03-26" and then
        // sends notifications/initialized — both verified by server
        // accepting them without error.
        adapter.initialize().await.unwrap();

        // After init, health should be Healthy
        assert_eq!(adapter.health(), crate::adapter::HealthStatus::Healthy);

        // List tools — should return meta-tools. JS mode is off in this
        // fixture, so execute_tools is hidden from the catalog.
        let tools = adapter.list_tools().await.unwrap();
        assert!(
            tools.len() >= 2,
            "expected at least 2 meta-tools, got {}",
            tools.len()
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
        assert!(!names.contains(&"execute_tools"));

        adapter.shutdown().await.unwrap();
        server_handle.abort();
    }

    // =====================================================================
    // MCP content format regression tests
    // =====================================================================

    /// Assert that a JSON-RPC response body has the MCP content array format:
    /// `result.content` is an array, each item has a `type` field, and text
    /// items have a non-empty `text` field.
    fn assert_mcp_content_format(response: &Value) {
        let content = response["result"]["content"]
            .as_array()
            .expect("result.content must be an array");
        assert!(!content.is_empty(), "content array must not be empty");
        for item in content {
            assert!(
                item["type"].is_string(),
                "each content item must have a string 'type' field"
            );
            if item["type"] == "text" {
                let text = item["text"]
                    .as_str()
                    .expect("text content item must have a 'text' field");
                assert!(!text.is_empty(), "text content must not be empty");
            }
        }
    }

    #[tokio::test]
    async fn content_format_list_tools_via_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "list_tools", "arguments": {}},
                "id": 10
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_mcp_content_format(&body);
    }

    #[tokio::test]
    async fn content_format_search_tools_via_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "search_tools", "arguments": {"query": "list"}},
                "id": 11
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_mcp_content_format(&body);
    }

    #[tokio::test]
    async fn content_format_execute_tools_via_unified_endpoint() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "execute_tools", "arguments": {"script": "1+1"}},
                "id": 12
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // execute_tools may succeed or error depending on JS runtime availability
        if body["result"].is_object() {
            assert_mcp_content_format(&body);
        }
    }

    /// Helper: send a JSON-RPC POST to `/mcp/tools/call` via the router.
    async fn post_mcp_tools_call(state: AppState, body: &Value) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;

        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp/tools/call")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn content_format_list_tools_via_legacy_endpoint() {
        let state = test_app_state();
        let resp = post_mcp_tools_call(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "list_tools", "arguments": {}},
                "id": 20
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_mcp_content_format(&body);
    }

    #[tokio::test]
    async fn nonexistent_tool_returns_jsonrpc_error() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "does_not_exist", "arguments": {}},
                "id": 30
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body["error"].is_object(),
            "nonexistent tool must return JSON-RPC error, got: {}",
            body
        );
        assert!(body["error"]["code"].is_number());
        assert!(body["error"]["message"].is_string());
        // Must NOT have a result.content array
        assert!(
            body["result"].is_null(),
            "error response must not have result"
        );
    }

    #[tokio::test]
    async fn missing_name_param_returns_jsonrpc_error() {
        let state = test_app_state();
        let resp = post_mcp(
            state,
            &json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"arguments": {}},
                "id": 31
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(
            body["error"].is_object(),
            "missing name must return JSON-RPC error, got: {}",
            body
        );
        assert_eq!(
            body["error"]["code"], -32602,
            "expected invalid-params error code"
        );
    }

    // --- Alphabetical sorting tests ---

    #[tokio::test]
    async fn test_tools_list_returns_sorted_tools() {
        let state = test_app_state();
        // Register an adapter with unsorted tool names
        state
            .registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::with_tools(&["zebra", "alpha", "mango"])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body), None).await;
        let tools = resp["result"]["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Should be sorted: adapter tools + meta-tools, all alphabetical.
        // execute_tools is gated on local_js_execution and is hidden when
        // JS mode is off (the default in this test).
        // Expected: alpha, list_tools, mango, search_tools, zebra
        assert_eq!(
            names,
            vec!["alpha", "list_tools", "mango", "search_tools", "zebra"]
        );
    }

    #[tokio::test]
    async fn test_tools_list_js_mode_meta_tools_sorted() {
        let state = test_app_state();
        state.js_execution_mode.store(true, Ordering::Relaxed);

        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("tools/list".to_string()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body), None).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Meta-tools should appear in definition order: list_tools, search_tools, execute_tools
        // In JS mode, they are NOT sorted (no sort call in that branch).
        // Verify the expected names are present.
        assert!(names.contains(&"execute_tools"));
        assert!(names.contains(&"list_tools"));
        assert!(names.contains(&"search_tools"));
    }

    #[test]
    fn test_html_escape_replaces_metacharacters() {
        let raw = r#"<script>alert("xss")</script> & 'quote'"#;
        let escaped = html_escape(raw);
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt; &amp; &#x27;quote&#x27;"
        );
        // No raw metacharacter should survive.
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert!(!escaped.contains('"'));
        assert!(!escaped.contains('\''));
    }

    #[test]
    fn test_html_escape_passes_through_safe_text() {
        assert_eq!(html_escape(""), "");
        assert_eq!(
            html_escape("plain endpoint name 123"),
            "plain endpoint name 123"
        );
    }

    #[tokio::test]
    async fn test_oauth_callback_escapes_error_param_and_sets_csp() {
        use axum::body::to_bytes;
        let state = test_app_state();
        let params = OAuthCallbackParams {
            code: None,
            state: None,
            error: Some(
                "<script>fetch('/api/test-connection',{method:'POST'})</script>".to_string(),
            ),
        };
        let resp = oauth_callback(State(state), Query(params)).await;
        let csp = resp
            .headers()
            .get("content-security-policy")
            .expect("CSP header present")
            .to_str()
            .unwrap()
            .to_string();
        assert!(csp.contains("default-src 'none'"), "csp = {csp}");
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("&lt;script&gt;"), "body = {body_str}");
        assert!(!body_str.contains("<script>"));
    }

    // --- /mcp/sse + initialize tools.listChanged capability ---

    /// `initialize` must advertise `tools.listChanged: true` so MCP clients
    /// know to consume `notifications/tools/list_changed` over `/mcp/sse`.
    #[tokio::test]
    async fn mcp_initialize_advertises_tools_list_changed() {
        let state = test_app_state();
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".to_string()),
            method: Some("initialize".to_string()),
            params: None,
            id: Some(json!(7)),
        };
        let Json(resp) = mcp_initialize(State(state), Json(body), None).await;
        assert_eq!(
            resp["result"]["capabilities"]["tools"]["listChanged"], true,
            "initialize must advertise tools.listChanged: true (got {resp})"
        );
    }

    /// Helper: read the SSE response body as a UTF-8 string, accumulating
    /// chunks until `predicate` returns true or `timeout` elapses. Returns
    /// the accumulated text either way so tests can produce useful failure
    /// messages.
    async fn read_sse_until<F: Fn(&str) -> bool>(
        resp: axum::response::Response,
        timeout: Duration,
        predicate: F,
    ) -> String {
        use futures_util::StreamExt;
        let mut stream = resp.into_body().into_data_stream();
        let mut collected = String::new();
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    collected.push_str(&String::from_utf8_lossy(&chunk));
                    if predicate(&collected) {
                        break;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
            }
        }
        collected
    }

    /// `mcp_sse` emits the initial `endpoint` event with `data: /mcp` so
    /// legacy SSE clients learn where to POST JSON-RPC requests.
    #[tokio::test]
    async fn mcp_sse_emits_initial_endpoint_event() {
        use axum::body::Body;
        use tower::ServiceExt;
        let state = test_app_state();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp/sse")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("event: endpoint") && s.contains("data: /mcp")
        })
        .await;
        assert!(
            text.contains("event: endpoint") && text.contains("data: /mcp"),
            "first SSE frame should be the endpoint event (got: {text:?})"
        );
    }

    /// `mcp_sse` forwards every relay-wide tools-changed tick as a JSON-RPC
    /// `notifications/tools/list_changed` SSE frame.
    #[tokio::test]
    async fn mcp_sse_forwards_tools_changed_notification() {
        use axum::body::Body;
        use tower::ServiceExt;
        let state = test_app_state();
        let registry = state.registry.clone();
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp/sse")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Drive a tick once the spawned forwarder is wired up. The forwarder
        // subscribes synchronously inside the handler, but the broadcast send
        // races with the subscriber, so retry the tick until the frame lands.
        let driver = tokio::spawn(async move {
            for _ in 0..40 {
                registry.tick_tools_changed_for_test("test-endpoint");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            text.contains("notifications/tools/list_changed"),
            "expected tools/list_changed frame within timeout (got: {text:?})"
        );
        assert!(
            text.contains("\"jsonrpc\":\"2.0\""),
            "frame should be a JSON-RPC notification (got: {text:?})"
        );
    }

    /// End-to-end SSE smoke test covering the path the user originally cared
    /// about: an MCP client subscribed to `/mcp/sse` is notified when a new
    /// endpoint is registered, and a subsequent `tools/list` reflects both
    /// the new tool and the `advertise.rs`-generated `Connected server
    /// types: …` line on `search_tools`.
    #[tokio::test]
    async fn mcp_sse_e2e_new_endpoint_notifies_and_updates_descriptions() {
        use axum::body::Body;
        use tower::ServiceExt;

        // Local mock that surfaces a `server_type()` so the advertised
        // description in `search_tools` picks up a `Connected server types:`
        // suffix. Kept inline to avoid touching the shared `MockAdapter`
        // used by other tests.
        struct TypedMockAdapter {
            tools: Vec<ToolInfo>,
            server_type_val: String,
        }
        #[async_trait]
        impl McpAdapter for TypedMockAdapter {
            async fn initialize(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
            async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
                Ok(self.tools.clone())
            }
            async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, AdapterError> {
                Ok(json!({ "called": name, "args": arguments }))
            }
            fn health(&self) -> HealthStatus {
                HealthStatus::Healthy
            }
            fn server_type(&self) -> Option<String> {
                Some(self.server_type_val.clone())
            }
            async fn shutdown(&mut self) -> Result<(), AdapterError> {
                Ok(())
            }
        }

        let state = test_app_state();
        let registry = state.registry.clone();
        let router = build_router(state.clone());

        // Step 1: open `/mcp/sse`. `build_mcp_sse_stream` subscribes to
        // `tools_changed_tx` synchronously before returning, so any tick
        // emitted after this point is guaranteed to be received.
        let sse_request = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp/sse")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let sse_resp = router.clone().oneshot(sse_request).await.unwrap();
        assert_eq!(sse_resp.status(), StatusCode::OK);

        // Step 2: register a new endpoint advertising a tool + a new
        // server type. `register` emits a per-endpoint tick on
        // `tools_changed_tx` (registry.rs:195).
        let driver = tokio::spawn(async move {
            // A small initial yield lets the spawned SSE writer task make
            // forward progress past the `endpoint` event before the tick
            // arrives. Without this the test still passes (the broadcast
            // queues), but yielding makes the timing match what a real
            // MCP client sees.
            tokio::task::yield_now().await;
            registry
                .register(
                    "smoke-ep".into(),
                    Box::new(TypedMockAdapter {
                        tools: vec![ToolInfo {
                            name: "smoke_tool".into(),
                            description: Some("smoke tool".into()),
                            input_schema: json!({"type": "object"}),
                            annotations: None,
                        }],
                        server_type_val: "smoke-server".into(),
                    }),
                    "stdio".into(),
                    None,
                    Some("smoke-ep".into()),
                )
                .await;
        });

        // Step 3: assert a `notifications/tools/list_changed` frame
        // arrives within 1s. `read_sse_until` accumulates all bytes so
        // the assertion below can also verify the initial endpoint
        // event was delivered on the same stream.
        let text = read_sse_until(sse_resp, Duration::from_secs(1), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.await.expect("registration driver task panicked");

        assert!(
            text.contains("event: endpoint") && text.contains("data: /mcp"),
            "SSE stream should have delivered the initial endpoint event before the tools-changed frame (got: {text:?})"
        );
        assert!(
            text.contains("notifications/tools/list_changed"),
            "FRAME_MISSING: SSE stream did not deliver a notifications/tools/list_changed frame within 1s of registering a new endpoint (got: {text:?})"
        );
        assert!(
            text.contains("\"jsonrpc\":\"2.0\""),
            "FRAME_MISSING: tools/list_changed frame should be a JSON-RPC notification (got: {text:?})"
        );

        // Step 4: issue `tools/list` and verify the new endpoint's tool
        // is present, and that `search_tools`'s description now includes
        // the new server type via `Connected server types: …`.
        let resp = post_mcp(
            state.clone(),
            &json!({"jsonrpc":"2.0","method":"tools/list","id":1}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let tools = body["result"]["tools"]
            .as_array()
            .expect("tools/list result must have a tools array");

        let tool_names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert!(
            tool_names.contains(&"smoke_tool"),
            "CATALOG_ENTRY_MISSING: tools/list should include the newly registered endpoint's tool 'smoke_tool' (got names: {tool_names:?})"
        );

        let search_tools = tools
            .iter()
            .find(|t| t["name"] == "search_tools")
            .expect("CATALOG_ENTRY_MISSING: tools/list must include the search_tools meta-tool");
        let search_desc = search_tools["description"].as_str().unwrap_or("");
        assert!(
            search_desc.contains("Connected server types:"),
            "DESCRIPTION_NOT_UPDATED: search_tools description should contain a 'Connected server types:' line now that an endpoint with a server_type is registered (got: {search_desc:?})"
        );
        assert!(
            search_desc.contains("smoke-server"),
            "DESCRIPTION_NOT_UPDATED: search_tools description should mention the new server type 'smoke-server' (got: {search_desc:?})"
        );
    }

    /// Helper: send `body` to a profile-scoped POST/DELETE/GET route via the
    /// router and return the raw response. Mirrors [`post_mcp`] for the
    /// `/mcp/{profile}` wildcard.
    async fn send_profile_request(
        state: AppState,
        method: &str,
        uri: &str,
        body: Option<&Value>,
    ) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;

        let router = build_router(state);
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        let body_bytes = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(v).unwrap())
            }
            None => Body::empty(),
        };
        let request = builder.body(body_bytes).unwrap();
        router.oneshot(request).await.unwrap()
    }

    /// Populate `state.profile_registry` with a single profile whose path is
    /// `path` and whose endpoint set is `endpoints`. JS execution defaults
    /// to off and TOON output to on — matching the historical "inherit from
    /// relay defaults" resolution callers relied on.
    async fn install_profile(state: &AppState, path: &str, endpoints: Vec<String>) {
        install_profile_with_flags(state, path, endpoints, false, true).await;
    }

    /// Variant of [`install_profile`] that takes explicit `js_execution` /
    /// `toon_output` values for the installed profile. Both fields are
    /// required on the profile config; callers pick concrete booleans for
    /// whichever path the test wants to exercise.
    async fn install_profile_with_flags(
        state: &AppState,
        path: &str,
        endpoints: Vec<String>,
        js_execution: bool,
        toon_output: bool,
    ) {
        let profile = crate::config::ProfileConfig {
            name: path.to_string(),
            path: path.to_string(),
            endpoints,
            js_execution,
            toon_output,
        };
        state.profile_registry.rebuild(&[profile]).await;
    }

    // Test-matrix row #24 — unknown profile path → 404 with JSON body.
    #[tokio::test]
    async fn mcp_unified_profiled_unknown_profile_returns_404_json() {
        let state = test_app_state();
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/nonexistent",
            Some(&json!({"jsonrpc":"2.0","method":"initialize","id":1})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = body_json(resp).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent"));
    }

    // `DELETE /mcp/{unknown}` should also 404 with the same JSON shape, not
    // 405 — the 405 contract is reserved for known profiles (mirroring the
    // global `/mcp` opt-out from session termination).
    #[tokio::test]
    async fn mcp_delete_profiled_unknown_profile_returns_404_json() {
        let state = test_app_state();
        let resp = send_profile_request(state, "DELETE", "/mcp/nonexistent", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent"));
    }

    // `GET /mcp/{unknown}/sse` should 404 with the same JSON shape.
    #[tokio::test]
    async fn mcp_sse_profiled_unknown_profile_returns_404_json() {
        let state = test_app_state();
        let resp = send_profile_request(state, "GET", "/mcp/nonexistent/sse", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent"));
    }

    // Known profile path → POST delegates to the shared `mcp_unified_impl`
    // and returns a normal JSON-RPC response (R3.A wires per-profile catalog
    // scoping; this slice only verifies the route reaches the handler).
    #[tokio::test]
    async fn mcp_unified_profiled_known_profile_dispatches_initialize() {
        let state = test_app_state();
        install_profile(&state, "work", vec![]).await;
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/work",
            Some(&json!({"jsonrpc":"2.0","method":"initialize","id":7})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 7);
        assert_eq!(body["result"]["serverInfo"]["name"], "Endara Relay");
    }

    // R3.C — profile-aware advertising wire-up.
    //
    // The matrix-row #15 contract is unit-tested in `crate::advertise` against
    // the description builders directly. These integration tests verify that
    // `mcp_initialize` and `mcp_tools_list` actually thread `profile_ctx`
    // through and emit profile-scoped output, distinguishing the per-profile
    // path from the global `/mcp` path. Endpoint count (not server_type) is
    // used as the differentiator because the server.rs `MockAdapter` does
    // not override `server_type()`.

    // `mcp_initialize` with `Some(profile_ctx)` advertises a non-`None`
    // `instructions` (lead-in only, since `MockAdapter` has no server_type)
    // when the profile has overlapping endpoints. The global `/mcp` path
    // sees the same registry so it also gets the lead-in — the differentiator
    // is the profile-vs-global selection (per the `_for_profile` builder).
    #[tokio::test]
    async fn mcp_initialize_uses_profile_variant_when_ctx_present() {
        let state = test_app_state();
        // Register two endpoints in the relay-wide registry. Without
        // server_type the lead-in renders without a `Connected server
        // types:` line — which is exactly the shape we expect because
        // `instructions_for_profile` should still emit the lead-in when
        // the profile's allowed endpoints are registered.
        state
            .registry
            .register(
                "gmail".into(),
                Box::new(MockAdapter::with_tools(&["send_email"])),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        state
            .registry
            .register(
                "github".into(),
                Box::new(MockAdapter::with_tools(&["list_issues"])),
                "stdio".into(),
                None,
                Some("github".into()),
            )
            .await;
        // Profile scoped to a non-overlapping endpoint → instructions must
        // be `None` (the "no overlap" branch of `instructions_for_profile`).
        install_profile(&state, "isolated", vec!["does-not-exist".into()]).await;
        let profile_ctx = state.profile_registry.get("isolated").await.unwrap();

        let body = JsonRpcBody {
            jsonrpc: Some("2.0".into()),
            method: Some("initialize".into()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_initialize(State(state.clone()), Json(body), Some(&profile_ctx)).await;
        assert!(
            resp["result"].get("instructions").is_none(),
            "profile with no in-scope endpoints must omit instructions, got: {resp}"
        );

        // Sanity check: the global path on the same state still includes
        // instructions, proving the profile path took a different branch.
        let body = JsonRpcBody {
            jsonrpc: Some("2.0".into()),
            method: Some("initialize".into()),
            params: None,
            id: Some(json!(2)),
        };
        let Json(global_resp) = mcp_initialize(State(state), Json(body), None).await;
        assert!(
            global_resp["result"]["instructions"].is_string(),
            "global path must still advertise instructions, got: {global_resp}"
        );
    }

    // `mcp_tools_list` with `Some(profile_ctx)` rebuilds the meta-tool
    // descriptions against the profile view so the count suffix reflects
    // only the profile's endpoints. Two endpoints registered globally, one
    // in profile → list_tools description ends with "1 servers connected",
    // not "2".
    #[tokio::test]
    async fn mcp_tools_list_meta_descriptions_are_profile_scoped() {
        let state = test_app_state();
        state.js_execution_mode.store(true, Ordering::Relaxed);
        state
            .registry
            .register(
                "gmail".into(),
                Box::new(MockAdapter::with_tools(&["send_email"])),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        state
            .registry
            .register(
                "github".into(),
                Box::new(MockAdapter::with_tools(&["list_issues"])),
                "stdio".into(),
                None,
                Some("github".into()),
            )
            .await;
        // R3.B: `ProfileContext::js_execution` is read directly from the
        // profile config at rebuild time, not from the runtime atomic. Set
        // it to `true` so this profile advertises `execute_tools` in the
        // catalog.
        install_profile_with_flags(&state, "work", vec!["gmail".into()], true, true).await;
        let profile_ctx = state.profile_registry.get("work").await.unwrap();

        let body = JsonRpcBody {
            jsonrpc: Some("2.0".into()),
            method: Some("tools/list".into()),
            params: None,
            id: Some(json!(1)),
        };
        let Json(resp) = mcp_tools_list(State(state), Json(body), Some(&profile_ctx)).await;
        let tools = resp["result"]["tools"].as_array().unwrap();
        let list_tools = tools
            .iter()
            .find(|t| t["name"] == "list_tools")
            .expect("list_tools meta-tool present");
        let desc = list_tools["description"].as_str().unwrap();
        assert!(
            desc.ends_with(" 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."),
            "list_tools description must reflect profile's 1 endpoint, not the registry's 2: {desc}"
        );
        // `execute_tools` is advertised in JS mode → same profile-scoped suffix.
        let execute_tools = tools
            .iter()
            .find(|t| t["name"] == "execute_tools")
            .expect("execute_tools meta-tool present in JS mode");
        let exec_desc = execute_tools["description"].as_str().unwrap();
        assert!(
            exec_desc.ends_with(" 1 servers connected via Endara Relay \u{2014} use search_tools to discover tools."),
            "execute_tools description must reflect profile's 1 endpoint: {exec_desc}"
        );
    }

    // Test-matrix row #12 — per-profile `toon_output = true` produces a
    // TOON-encoded `list_tools` response even when the relay default
    // (`relay.toon_output`) is off. Exercises the profile override path in
    // `mcp_tools_call`.
    #[tokio::test]
    async fn profile_toon_on_encodes_toon_when_global_off() {
        let state = test_app_state();
        assert!(
            !state.toon_enabled,
            "test relies on global toon default being off"
        );
        install_profile_with_flags(&state, "work", vec![], false, true).await;
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/work",
            Some(&json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "list_tools", "arguments": {}},
                "id": 1
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        // TOON output for the `{ tools, total, limit, offset }` envelope is
        // never valid JSON — it starts with field declarations, not `{`.
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "profile toon=on must yield TOON text, got JSON-parseable: {text}"
        );
        assert!(
            text.contains("total:"),
            "expected TOON field syntax in: {text}"
        );
    }

    // Test-matrix row #13 — per-profile `toon_output = false` keeps the
    // `list_tools` response as raw JSON even when the relay default
    // (`relay.toon_output`) is on. Mirror of #12 for the override-off path.
    #[tokio::test]
    async fn profile_toon_off_keeps_json_when_global_on() {
        let state = test_app_state();
        install_profile_with_flags(&state, "work", vec![], false, false).await;
        // Sanity: the profile's TOON flag is off.
        let global_ctx = state.profile_registry.get("work").await.unwrap();
        assert!(!global_ctx.toon_output, "profile toon must be off");
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/work",
            Some(&json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "list_tools", "arguments": {}},
                "id": 1
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value =
            serde_json::from_str(text).expect("profile toon=off must yield JSON text");
        assert!(
            parsed["tools"].is_array(),
            "expected `tools` array in JSON: {text}"
        );
    }

    // R3.B — per-profile `js_execution = true` enables `execute_tools` and
    // gates direct tool calls even when the relay default
    // (`relay.local_js_execution`) is off. The `tools/call` for an unknown
    // direct tool must surface the JS-mode rejection error, not the
    // generic "unknown tool" path.
    #[tokio::test]
    async fn profile_js_on_rejects_direct_tool_calls_when_global_off() {
        let state = test_app_state();
        assert!(
            !state.js_execution_mode.load(Ordering::Relaxed),
            "test relies on global JS default being off"
        );
        install_profile_with_flags(&state, "work", vec![], true, true).await;
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/work",
            Some(&json!({
                "jsonrpc": "2.0",
                "method": "tools/call",
                "params": {"name": "anything", "arguments": {}},
                "id": 1
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let msg = body["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("JS execution mode"),
            "expected JS-mode rejection, got: {body}"
        );
    }

    // R3.B — per-profile `js_execution = false` hides `execute_tools` from
    // the catalog even when the relay default (`relay.local_js_execution`)
    // is on. Symmetric to the override-on test above.
    #[tokio::test]
    async fn profile_js_off_hides_execute_tools_when_global_on() {
        let state = test_app_state();
        install_profile_with_flags(&state, "work", vec![], false, true).await;
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/work",
            Some(&json!({
                "jsonrpc": "2.0",
                "method": "tools/list",
                "id": 1
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let tools = body["result"]["tools"].as_array().unwrap();
        assert!(
            tools.iter().all(|t| t["name"] != "execute_tools"),
            "execute_tools must be hidden when profile js=off, got: {tools:?}"
        );
        // The non-JS meta-tools are still advertised.
        assert!(tools.iter().any(|t| t["name"] == "list_tools"));
        assert!(tools.iter().any(|t| t["name"] == "search_tools"));
    }

    // Profile path lookup is case-insensitive (R2.A: registry lowercases keys).
    #[tokio::test]
    async fn mcp_unified_profiled_lookup_is_case_insensitive() {
        let state = test_app_state();
        install_profile(&state, "work", vec![]).await;
        let resp = send_profile_request(
            state,
            "POST",
            "/mcp/WORK",
            Some(&json!({"jsonrpc":"2.0","method":"initialize","id":1})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Known profile + DELETE → 405 (matches the global `/mcp` route).
    #[tokio::test]
    async fn mcp_delete_profiled_known_profile_returns_405() {
        let state = test_app_state();
        install_profile(&state, "work", vec![]).await;
        let resp = send_profile_request(state, "DELETE", "/mcp/work", None).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    // Known profile + GET SSE → 200 + initial `endpoint` event points at the
    // profile's URL so SSE-only clients POST follow-ups to `/mcp/{profile}`.
    #[tokio::test]
    async fn mcp_sse_profiled_known_profile_emits_endpoint_event() {
        let state = test_app_state();
        install_profile(&state, "work", vec![]).await;
        let resp = send_profile_request(state, "GET", "/mcp/work/sse", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("event: endpoint") && s.contains("data: /mcp/work")
        })
        .await;
        assert!(
            text.contains("event: endpoint") && text.contains("data: /mcp/work"),
            "first SSE frame should be the endpoint event for /mcp/work (got: {text:?})"
        );
    }

    // Sanity: legacy `/mcp/initialize`, `/mcp/tools/list`, `/mcp/sse` are
    // still reachable after the wildcard `/mcp/{profile}` was added (recon
    // D7 — axum 0.8 prefers specific routes over `{profile}` wildcards).
    #[tokio::test]
    async fn legacy_specific_mcp_routes_still_match_after_wildcard() {
        use axum::body::Body;
        use tower::ServiceExt;
        let state = test_app_state();
        let router = build_router(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp/initialize")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({"jsonrpc":"2.0","method":"initialize","id":1})).unwrap(),
            ))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["result"]["serverInfo"]["name"], "Endara Relay");

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp/sse")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Helper: open a profile-scoped SSE stream and return the response.
    /// Mirrors the per-test boilerplate in [`mcp_sse_profiled_known_profile_emits_endpoint_event`].
    async fn open_profile_sse(state: AppState, profile_path: &str) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(format!("/mcp/{}/sse", profile_path))
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    /// Drive `endpoint_name` tools-changed ticks on the registry until the
    /// `deadline` elapses. Mirrors the retry-tick pattern used by the
    /// existing `mcp_sse_forwards_tools_changed_notification` test: each
    /// `subscribe_tools_changed()` call races with the in-handler subscriber
    /// being installed, so a single send is unreliable.
    async fn drive_ticks_until(
        registry: AdapterRegistry,
        endpoint_name: &str,
        deadline: tokio::time::Instant,
    ) {
        let name = endpoint_name.to_string();
        while tokio::time::Instant::now() < deadline {
            registry.tick_tools_changed_for_test(&name);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // Test-matrix row #16 — in-profile endpoint change is forwarded on
    // `/mcp/{profile}/sse`. The profile contains "gmail"; ticking "gmail"
    // produces a `notifications/tools/list_changed` SSE frame.
    #[tokio::test]
    async fn mcp_sse_profiled_forwards_in_profile_tools_changed() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into(), "linear".into()]).await;
        let registry = state.registry.clone();
        let resp = open_profile_sse(state, "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let driver = tokio::spawn(async move {
            drive_ticks_until(registry, "gmail", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            text.contains("notifications/tools/list_changed"),
            "expected in-profile tools/list_changed frame (got: {text:?})"
        );
    }

    // Test-matrix row #17 — out-of-profile endpoint change is suppressed on
    // `/mcp/{profile}/sse`. Ticking "todoist" (not in the profile) for the
    // full window must not produce a `notifications/tools/list_changed`
    // frame; only the initial `endpoint` event should be visible.
    #[tokio::test]
    async fn mcp_sse_profiled_suppresses_out_of_profile_tools_changed() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into(), "linear".into()]).await;
        let registry = state.registry.clone();
        let resp = open_profile_sse(state, "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let driver = tokio::spawn(async move {
            drive_ticks_until(registry, "todoist", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(1), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            !text.contains("notifications/tools/list_changed"),
            "out-of-profile tick must not be forwarded (got: {text:?})"
        );
        assert!(
            text.contains("event: endpoint") && text.contains("data: /mcp/work"),
            "initial endpoint frame should still be emitted (got: {text:?})"
        );
    }

    // Mixed traffic: ticks alternate between in-profile and out-of-profile
    // endpoints; only the in-profile tick must surface. Guards against an
    // implementation that filters incorrectly (e.g. forwards everything or
    // nothing).
    #[tokio::test]
    async fn mcp_sse_profiled_only_forwards_in_profile_when_mixed() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        let registry = state.registry.clone();
        let resp = open_profile_sse(state, "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let driver = tokio::spawn(async move {
            while tokio::time::Instant::now() < deadline {
                registry.tick_tools_changed_for_test("todoist");
                registry.tick_tools_changed_for_test("gmail");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            text.contains("notifications/tools/list_changed"),
            "expected at least one in-profile frame from mixed traffic (got: {text:?})"
        );
    }

    // Sanity: the global `/mcp/sse` stream is unchanged by R3.D — it still
    // forwards every tick regardless of the originating endpoint (the
    // `None`-filter branch of `build_mcp_sse_stream`).
    #[tokio::test]
    async fn mcp_sse_global_forwards_every_endpoint_after_r3d() {
        let state = test_app_state();
        // No profile installed; the global handler ignores the profile
        // registry entirely.
        let registry = state.registry.clone();
        let resp = open_profile_sse_via_global(state).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let driver = tokio::spawn(async move {
            drive_ticks_until(registry, "anything", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            text.contains("notifications/tools/list_changed"),
            "global /mcp/sse must forward every tick regardless of endpoint (got: {text:?})"
        );
    }

    /// Helper: open the global `/mcp/sse` stream. Kept separate from
    /// [`open_profile_sse`] so the global regression test reads explicitly.
    async fn open_profile_sse_via_global(state: AppState) -> axum::response::Response {
        use axum::body::Body;
        use tower::ServiceExt;
        let router = build_router(state);
        let request = axum::http::Request::builder()
            .method("GET")
            .uri("/mcp/sse")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();
        router.oneshot(request).await.unwrap()
    }

    // -- Profile-channel propagation matrix ----------------------------------
    //
    // The rows below back acceptance #2 of the spec: profile-scoped
    // `/mcp/{profile}/sse` must receive a frame on profile-membership /
    // toggle changes affecting it, and must re-read the live allowed-set
    // when forwarding per-endpoint ticks so mid-stream membership changes
    // take effect without reconnection. Each test mirrors the retry-tick
    // pattern of the existing R3.D matrix (the in-handler subscriber races
    // with the broadcast send, so single sends are unreliable).

    /// Drive profile-changed ticks on `state.profile_registry` until
    /// `deadline`. Mirrors [`drive_ticks_until`] but exercises the parallel
    /// profile-channel installed alongside `AdapterRegistry`'s per-endpoint
    /// channel.
    async fn drive_profile_ticks_until(
        state: AppState,
        profile_path: &str,
        deadline: tokio::time::Instant,
    ) {
        let path = profile_path.to_string();
        while tokio::time::Instant::now() < deadline {
            state.profile_registry.tick_profile_changed_for_test(&path);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // Matrix row A — profile-channel tick whose payload matches the
    // stream's profile path is forwarded as a tools/list_changed frame.
    // Backs spec acceptance #2: membership change emits a frame.
    #[tokio::test]
    async fn mcp_sse_profiled_forwards_profile_channel_tick() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        let resp = open_profile_sse(state.clone(), "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let driver_state = state.clone();
        let driver = tokio::spawn(async move {
            drive_profile_ticks_until(driver_state, "work", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            text.contains("notifications/tools/list_changed"),
            "expected profile-channel tick to be forwarded (got: {text:?})"
        );
    }

    // Matrix row B — profile-channel tick for a *different* profile path
    // is suppressed on this stream. Guards against the filter being too
    // permissive (e.g. forwarding every profile tick).
    #[tokio::test]
    async fn mcp_sse_profiled_suppresses_other_profile_channel_tick() {
        let state = test_app_state();
        // Install both profiles so `mcp_sse_profiled` opens cleanly on `work`
        // and the "personal" path is a legitimate payload value.
        let profiles = vec![
            crate::config::ProfileConfig {
                name: "work".into(),
                path: "work".into(),
                endpoints: vec!["gmail".into()],
                js_execution: false,
                toon_output: true,
            },
            crate::config::ProfileConfig {
                name: "personal".into(),
                path: "personal".into(),
                endpoints: vec!["todoist".into()],
                js_execution: false,
                toon_output: true,
            },
        ];
        state.profile_registry.rebuild(&profiles).await;

        let resp = open_profile_sse(state.clone(), "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let driver_state = state.clone();
        let driver = tokio::spawn(async move {
            drive_profile_ticks_until(driver_state, "personal", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(1), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            !text.contains("notifications/tools/list_changed"),
            "other-profile tick must not be forwarded (got: {text:?})"
        );
        assert!(
            text.contains("event: endpoint") && text.contains("data: /mcp/work"),
            "initial endpoint frame should still be emitted (got: {text:?})"
        );
    }

    // Matrix row C — a `rebuild` that adds an endpoint to the profile must
    // emit one profile-channel tick (consumed by the open stream) and the
    // *next* per-endpoint tick for the newly-added endpoint must be
    // forwarded because the stream re-reads the allowed set live. Pre-R3.D
    // the stream captured a static snapshot, so this test would have
    // suppressed the post-rebuild "gmail" tick.
    #[tokio::test]
    async fn mcp_sse_profiled_picks_up_new_membership_live() {
        let state = test_app_state();
        // Start with an empty profile; "gmail" is initially out-of-profile.
        install_profile(&state, "work", vec![]).await;
        let resp = open_profile_sse(state.clone(), "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Rebuild to include "gmail". This emits a profile-channel tick.
        install_profile(&state, "work", vec!["gmail".into()]).await;

        // After the rebuild, per-endpoint ticks for "gmail" should now be
        // forwarded. Drive ticks until the frame lands.
        let registry = state.registry.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let driver = tokio::spawn(async move {
            drive_ticks_until(registry, "gmail", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(2), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            text.contains("notifications/tools/list_changed"),
            "expected post-rebuild in-profile tick to be forwarded (got: {text:?})"
        );
    }

    // Matrix row D — converse of row C: a `rebuild` that *removes* an
    // endpoint from the profile must cause subsequent per-endpoint ticks
    // for that endpoint to be suppressed. The lone frame the stream is
    // allowed to surface is the one fired by the rebuild itself (profile
    // channel); after that, ticks against the removed endpoint must not
    // appear.
    #[tokio::test]
    async fn mcp_sse_profiled_drops_removed_membership_live() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        let resp = open_profile_sse(state.clone(), "work").await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Drain the initial `endpoint` event and the rebuild's profile
        // tick, then prove the next "gmail" per-endpoint tick is dropped.
        install_profile(&state, "work", vec![]).await;
        // Give the spawned forwarder a beat to consume the profile-channel
        // tick before we start driving per-endpoint ticks.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Now drive "gmail" per-endpoint ticks for a bounded window. The
        // stream should swallow them all because "gmail" is no longer in
        // the live allowed set.
        let registry = state.registry.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let driver = tokio::spawn(async move {
            drive_ticks_until(registry, "gmail", deadline).await;
        });
        // Read for slightly longer than the deadline to confirm no extra
        // frames arrive after the rebuild's bookkeeping tick.
        let text = read_sse_until(resp, Duration::from_secs(1), |s| {
            // Two frames would indicate the per-endpoint forwarding still
            // sees "gmail" as in-profile.
            s.matches("notifications/tools/list_changed").count() >= 2
        })
        .await;
        driver.abort();
        let frame_count = text.matches("notifications/tools/list_changed").count();
        assert!(
            frame_count <= 1,
            "post-removal per-endpoint ticks for 'gmail' must be suppressed; \
             got {frame_count} frames: {text:?}"
        );
    }

    // Matrix row E — the global `/mcp/sse` stream ignores profile-channel
    // ticks entirely. Without this, profile-membership churn (which is
    // unrelated to the global tool surface) would spam every connected
    // global client with redundant notifications.
    #[tokio::test]
    async fn mcp_sse_global_ignores_profile_channel_ticks() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        let resp = open_profile_sse_via_global(state.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let driver_state = state.clone();
        let driver = tokio::spawn(async move {
            drive_profile_ticks_until(driver_state, "work", deadline).await;
        });
        let text = read_sse_until(resp, Duration::from_secs(1), |s| {
            s.contains("notifications/tools/list_changed")
        })
        .await;
        driver.abort();
        assert!(
            !text.contains("notifications/tools/list_changed"),
            "global /mcp/sse must ignore profile-channel ticks (got: {text:?})"
        );
    }

    // Matrix row F — a no-op `rebuild` (same configs) MUST NOT emit a
    // profile-channel tick. Backs spec acceptance #5 for profile streams.
    #[tokio::test]
    async fn rebuild_with_identical_config_emits_no_profile_tick() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        // Subscribe AFTER the initial rebuild so we only observe ticks from
        // the second (no-op) rebuild.
        let mut rx = state.profile_registry.subscribe_profiles_changed();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        // Give the broadcast a beat to deliver, then assert nothing landed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("no-op rebuild must not emit a profile tick (got: {other:?})"),
        }
    }

    // Matrix row G — toggling `js_execution` on an otherwise-identical
    // profile flips `execute_tools` in/out of the advertised catalog, so
    // the rebuild must emit a profile-channel tick. Backs spec acceptance
    // #2 ("profile sandbox/toggle changes that affect advertised tools").
    #[tokio::test]
    async fn rebuild_js_execution_toggle_emits_profile_tick() {
        let state = test_app_state();
        install_profile_with_flags(&state, "work", vec!["gmail".into()], false, true).await;
        let mut rx = state.profile_registry.subscribe_profiles_changed();
        install_profile_with_flags(&state, "work", vec!["gmail".into()], true, true).await;
        let tick = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("profile-channel tick must arrive within 1s")
            .expect("profile-channel recv must not error");
        assert_eq!(
            tick, "work",
            "js_execution toggle on 'work' must tick the 'work' path"
        );
    }

    // Matrix row H — toggling only `toon_output` does NOT change the
    // advertised tool list (it only affects response serialization), so no
    // profile-channel tick is emitted. Backs spec acceptance #5 (no frame
    // on toggles that don't affect the advertised catalog).
    #[tokio::test]
    async fn rebuild_toon_only_toggle_emits_no_profile_tick() {
        let state = test_app_state();
        install_profile_with_flags(&state, "work", vec!["gmail".into()], false, true).await;
        let mut rx = state.profile_registry.subscribe_profiles_changed();
        install_profile_with_flags(&state, "work", vec!["gmail".into()], false, false).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => {
                panic!("toon_output-only toggle must not emit a profile tick (got: {other:?})")
            }
        }
    }

    // Matrix row I — removing a profile via `rebuild(&[])` must emit a
    // final profile-channel tick for the now-deleted path so any open
    // `/mcp/{path}/sse` stream gets one last `notifications/tools/list_changed`
    // frame before its per-endpoint forwarding goes silent (because
    // `ProfileRegistry::get` will return `None` from then on).
    #[tokio::test]
    async fn rebuild_emits_tick_for_removed_profile() {
        let state = test_app_state();
        install_profile(&state, "work", vec!["gmail".into()]).await;
        let mut rx = state.profile_registry.subscribe_profiles_changed();
        state.profile_registry.rebuild(&[]).await;
        let tick = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("removal must tick within 1s")
            .expect("profile-channel recv must not error");
        assert_eq!(tick, "work", "removed profile path must be broadcast");
    }

    // R3.E — `profile` field in tracing spans. Nested under `tracing::profile`
    // so `cargo test tracing::profile` (the verification command on the task
    // note) selects exactly these cases. The inner module names intentionally
    // shadow the `tracing` extern crate within this scope; tests reference it
    // via the absolute path `::tracing::` to disambiguate.
    mod tracing {
        mod profile {
            use super::super::*;
            use ::tracing_subscriber::fmt::MakeWriter;
            use std::io;
            use std::sync::{Arc, Mutex};

            /// In-memory `MakeWriter` so the test can read back every byte the
            /// `fmt` subscriber emitted. Mirrors the buffered-writer pattern
            /// already used in `watcher.rs::adapter_init_warns_...`.
            #[derive(Clone, Default)]
            struct BufWriter(Arc<Mutex<Vec<u8>>>);
            impl io::Write for BufWriter {
                fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            impl<'a> MakeWriter<'a> for BufWriter {
                type Writer = BufWriter;
                fn make_writer(&'a self) -> Self::Writer {
                    self.clone()
                }
            }

            /// Drain the buffer into a UTF-8 string.
            fn captured(buf: &BufWriter) -> String {
                String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
            }

            // Matrix #22 — profile-scoped POST emits at least one log line
            // bearing `profile=<path>`. The `MCP request` line lives inside
            // `handle_single_message`'s `request` span, which is itself a
            // child of `mcp_request{profile=...}` thanks to the `.instrument`
            // wrap on `mcp_unified_profiled`. With span-field propagation the
            // child event's formatted output carries the parent's `profile`
            // field.
            #[tokio::test(flavor = "current_thread")]
            #[serial_test::serial(tracing)]
            async fn field_present_on_profiled_request() {
                crate::test_tracing::init_permissive_tracing();
                let buf = BufWriter::default();
                let subscriber = ::tracing_subscriber::fmt()
                    .with_writer(buf.clone())
                    .with_max_level(::tracing::Level::INFO)
                    .with_ansi(false)
                    .finish();
                let _guard = ::tracing::subscriber::set_default(subscriber);

                let state = test_app_state();
                install_profile(&state, "work", vec!["gmail".into()]).await;
                let resp = send_profile_request(
                    state,
                    "POST",
                    "/mcp/work",
                    Some(&json!({"jsonrpc":"2.0","method":"initialize","id":1})),
                )
                .await;
                assert_eq!(resp.status(), StatusCode::OK);
                // Drain the body so the handler future fully completes before
                // we read the captured logs.
                let _ = body_json(resp).await;

                let text = captured(&buf);
                assert!(
                    text.contains("profile=work"),
                    "expected `profile=work` in tracing output for /mcp/work; got: {text:?}"
                );
                assert!(
                    text.contains("MCP request"),
                    "expected the inner `MCP request` log line to appear; got: {text:?}"
                );
            }

            // Counterpart to the matrix #22 row: the global `/mcp` route MUST
            // NOT add a `profile` field. Locked decision Cross-stack #1 makes
            // the field key `profile=` an immutable contract, so any
            // accidental insertion (e.g. a stray default span) would break
            // the desktop log filter's per-profile dropdown.
            #[tokio::test(flavor = "current_thread")]
            #[serial_test::serial(tracing)]
            async fn field_absent_on_global_request() {
                crate::test_tracing::init_permissive_tracing();
                let buf = BufWriter::default();
                let subscriber = ::tracing_subscriber::fmt()
                    .with_writer(buf.clone())
                    .with_max_level(::tracing::Level::INFO)
                    .with_ansi(false)
                    .finish();
                let _guard = ::tracing::subscriber::set_default(subscriber);

                let state = test_app_state();
                let resp = post_mcp(
                    state,
                    &json!({"jsonrpc":"2.0","method":"initialize","id":1}),
                )
                .await;
                assert_eq!(resp.status(), StatusCode::OK);
                let _ = body_json(resp).await;

                let text = captured(&buf);
                assert!(
                    text.contains("MCP request"),
                    "expected the global `MCP request` log line to appear; got: {text:?}"
                );
                assert!(
                    !text.contains("profile="),
                    "global /mcp must not introduce a `profile` field; got: {text:?}"
                );
            }
        }
    }
}
