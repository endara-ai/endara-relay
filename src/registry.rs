use crate::adapter::stdio::iso8601_now;
use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::events::{current_request_context, ToolCallEvent, ToolCallEventBus};
use crate::observability::pipeline::{CaptureRecord, CapturedPayloads, Observability};
use crate::observability::store::CallRecord;
use crate::prefix;
use async_trait::async_trait;
use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::paths::{Location, LocationSegment};
use jsonschema::{ValidationError, Validator};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

/// A registered adapter with its metadata.
pub struct RegisteredAdapter {
    pub adapter: Box<dyn McpAdapter>,
    pub transport: String,
    pub description: Option<String>,
    pub tool_prefix: Option<String>,
    pub last_activity: Option<Instant>,
    pub disabled: bool,
    pub disabled_tools: HashSet<String>,
    /// Per-adapter cache of the most recent successful `list_tools()` result.
    /// Purely event-driven (no TTL); cleared by registry invalidation methods
    /// or when the adapter is swapped/restarted.
    pub(crate) tool_cache: RwLock<Option<Vec<ToolInfo>>>,
    /// Coalesces concurrent cache misses so only one `list_tools()` call is
    /// in-flight per adapter at a time.
    pub(crate) tool_cache_populate_lock: Mutex<()>,
    /// Abort handle for the background task that listens to the adapter's
    /// `subscribe_tools_changed` broadcast and triggers cache invalidation
    /// on each tick. `None` when the adapter does not expose a receiver.
    pub(crate) tools_changed_task: Option<AbortHandle>,
}

impl RegisteredAdapter {
    /// List tools using the per-adapter cache. On a healthy hit, returns a
    /// clone of the cached vector without calling the underlying adapter.
    /// On any other case (cache miss, or adapter not yet `Healthy`), takes
    /// the populate lock and re-checks state under it so a thundering herd
    /// of concurrent callers cannot drive the underlying `list_tools` (and,
    /// for OAuth adapters, its refresh) more than once per coalesced group.
    ///
    /// When the adapter is **not** `HealthStatus::Healthy` (for example, an
    /// OAuth adapter pinned in `Refreshing` reports `Starting`) the cache is
    /// bypassed and the call is delegated straight to the adapter. This
    /// prevents a stale cached vector — captured back when the adapter was
    /// healthy — from masking a stuck endpoint and from short-circuiting the
    /// reactive 401-driven refresh path inside the adapter's `list_tools`.
    /// The cache is **only** populated from the bypass branch when the
    /// adapter has fully recovered to `Healthy` by the time the call
    /// completes; if it is still unhealthy the result is returned verbatim
    /// without writing the cache, preserving the invariant that an
    /// unhealthy adapter never poisons the cache.
    pub async fn cached_list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
        // Fast path: healthy adapter with a populated cache returns
        // without taking the populate lock or calling the adapter.
        if matches!(self.adapter.health(), HealthStatus::Healthy) {
            if let Some(cached) = self.tool_cache.read().await.as_ref() {
                return Ok(cached.clone());
            }
        }
        // Slow path: serialize concurrent callers behind the populate lock
        // so a herd against a recovering or unhealthy adapter cannot drive
        // the underlying `list_tools` independently per caller.
        let _populate = self.tool_cache_populate_lock.lock().await;
        // Re-check under the lock: a prior holder may have driven recovery
        // and populated the cache while we were parked.
        if matches!(self.adapter.health(), HealthStatus::Healthy) {
            if let Some(cached) = self.tool_cache.read().await.as_ref() {
                return Ok(cached.clone());
            }
            let tools = self.adapter.list_tools().await?;
            *self.tool_cache.write().await = Some(tools.clone());
            return Ok(tools);
        }
        // Still not `Healthy` under the lock: delegate straight to the
        // adapter (its own `list_tools` may drive a reactive refresh) and
        // only populate the cache if the adapter has fully recovered to
        // `Healthy` by the time the call completes — this lets a single
        // coalesced caller's recovered result short-circuit the rest of
        // the herd while preserving the invariant that a still-unhealthy
        // adapter never poisons the cache.
        let tools = self.adapter.list_tools().await?;
        if matches!(self.adapter.health(), HealthStatus::Healthy) {
            *self.tool_cache.write().await = Some(tools.clone());
        }
        Ok(tools)
    }
}

/// Cached catalog type: `(tool_list, reverse_lookup_map)`.
type CatalogCache = (Vec<ToolInfo>, HashMap<String, (String, String)>);

/// Compiled JSON Schema validators keyed by prefixed tool name. Lazily
/// populated on the first `route_tool_call` for each tool and cleared by
/// [`AdapterRegistry::invalidate_catalog_cache`]. `None` means the tool has
/// no useful schema to validate against (degenerate schema, or one that
/// failed to compile) and the call passes through unvalidated.
type CompiledSchemaCache = HashMap<String, Option<Arc<Validator>>>;

/// Thread-safe registry of MCP adapters keyed by endpoint name.
#[derive(Clone)]
pub struct AdapterRegistry {
    adapters: Arc<RwLock<HashMap<String, RegisteredAdapter>>>,
    catalog_cache: Arc<RwLock<Option<CatalogCache>>>,
    /// Monotonically-increasing counter bumped on every catalog-affecting
    /// mutation. Downstream consumers (e.g. `MetaToolHandler`'s search index
    /// cache) can use this as an authoritative invalidation signal without
    /// needing to diff catalog contents.
    catalog_generation: Arc<AtomicU64>,
    /// Relay-wide tools-changed broadcast. Fan-in for every per-endpoint
    /// `subscribe_tools_changed` tick observed by
    /// [`AdapterRegistry::tools_changed_listener_loop`]. The payload is the
    /// endpoint name that emitted the change, so downstream consumers can
    /// filter by endpoint (e.g. per-profile SSE handlers that only forward
    /// `notifications/tools/list_changed` when an endpoint inside their
    /// profile changes). Consumers that don't care about origin (e.g. the
    /// global `/mcp/sse` handler) forward unconditionally.
    tools_changed_tx: broadcast::Sender<String>,
    /// Optional shared [`ToolCallEventBus`] used by [`Self::route_tool_call`]
    /// to publish `started` + `failed` pairs for early-rejection branches
    /// that short-circuit before reaching an adapter's `call_tool` (unknown
    /// prefix, missing adapter, disabled endpoint, unhealthy endpoint,
    /// disabled tool). `None` by default so the many test-only call sites
    /// that construct a bare registry without a bus keep compiling; wired in
    /// by `main.rs` via [`Self::with_event_bus`] so the overlay still gets
    /// the same `Started` → `Failed` transition adapters produce for calls
    /// that do reach the network.
    event_bus: Option<ToolCallEventBus>,
    /// Lazily-populated cache of compiled `inputSchema` validators, keyed by
    /// prefixed tool name. Populated on first validation in
    /// [`Self::route_tool_call`] and cleared alongside the catalog cache in
    /// [`Self::invalidate_catalog_cache`]. See [`CompiledSchemaCache`].
    schema_cache: Arc<RwLock<CompiledSchemaCache>>,
    /// When `true` (the effective default), [`Self::route_tool_call`]
    /// validates `tools/call` arguments against the tool's `inputSchema`
    /// before dispatch. Hot-reloadable: `ConfigWatcher` calls
    /// [`Self::set_validate_inputs`] on every reload, mirroring the
    /// `js_execution_mode` flag pattern.
    validate_inputs: Arc<AtomicBool>,
    /// Optional observability ingestion pipeline (R4). When wired via
    /// [`Self::with_observability`] and enabled, [`Self::route_tool_call`]
    /// captures each dispatched `tools/call` (args + result + timing +
    /// `current_request_context()`) and enqueues it without blocking the
    /// request path. `None` by default so the many test-only call sites that
    /// construct a bare registry keep compiling.
    observability: Option<Observability>,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterRegistry {
    /// Create a new, empty adapter registry.
    pub fn new() -> Self {
        let (tools_changed_tx, _) = broadcast::channel(16);
        Self {
            adapters: Arc::new(RwLock::new(HashMap::new())),
            catalog_cache: Arc::new(RwLock::new(None)),
            catalog_generation: Arc::new(AtomicU64::new(0)),
            tools_changed_tx,
            event_bus: None,
            schema_cache: Arc::new(RwLock::new(HashMap::new())),
            validate_inputs: Arc::new(AtomicBool::new(true)),
            observability: None,
        }
    }

    /// Set the initial value of the input-validation toggle. Builder style so
    /// the many `AdapterRegistry::new()` call sites keep compiling; `main.rs`
    /// wires the effective default (`relay.validate_inputs`, defaulting to
    /// `true`) at startup.
    pub fn with_validate_inputs(self, enabled: bool) -> Self {
        self.validate_inputs.store(enabled, Ordering::Relaxed);
        self
    }

    /// Update the input-validation toggle at runtime. Called by
    /// `ConfigWatcher` on every config reload so the flag is hot-reloadable
    /// without restarting the relay. Mirrors the `js_execution_mode` pattern.
    pub fn set_validate_inputs(&self, enabled: bool) {
        self.validate_inputs.store(enabled, Ordering::Relaxed);
    }

    /// Current value of the input-validation toggle.
    pub fn validate_inputs(&self) -> bool {
        self.validate_inputs.load(Ordering::Relaxed)
    }

    /// Attach a shared [`ToolCallEventBus`] so [`Self::route_tool_call`]
    /// publishes a `Started` + `Failed` pair for every early-rejection
    /// branch (unknown prefix, missing/disabled/unhealthy endpoint,
    /// disabled tool). The same bus instance must be wired into adapters
    /// via [`McpAdapter::set_event_bus`] so the overlay sees a single
    /// fan-out for both pre-adapter and adapter-side failures. Builder
    /// style so the existing many-call-sites `AdapterRegistry::new()`
    /// continues to compile unchanged.
    pub fn with_event_bus(mut self, bus: ToolCallEventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Attach the observability ingestion pipeline (R4) so
    /// [`Self::route_tool_call`] captures each dispatched `tools/call`. Builder
    /// style so the existing `AdapterRegistry::new()` call sites keep compiling;
    /// `main.rs` wires the handle constructed from `relay.observability`.
    pub fn with_observability(mut self, observability: Observability) -> Self {
        self.observability = Some(observability);
        self
    }

    /// Borrow the wired observability handle, if any. Lets management/API
    /// (R5) and the retention/cascade driver (R6) reach the shared `Store`,
    /// `PayloadStore`, overflow stats and capture entrypoint through the
    /// registry the management state already holds. Unused until R5/R6 wire
    /// their consumers; the allow keeps the binary target warning-clean.
    #[allow(dead_code)]
    pub fn observability(&self) -> Option<&Observability> {
        self.observability.as_ref()
    }

    /// Subscribe to the relay-wide tools-changed broadcast. Each `recv()`
    /// yields the endpoint name that emitted a
    /// `notifications/tools/list_changed` tick since the previous receive.
    /// Mirrors the per-adapter [`McpAdapter::subscribe_tools_changed`]
    /// policy: `Lagged` is also surfaced as a tick (the endpoint name is
    /// lost in that case — consumers should forward unconditionally), and
    /// `Closed` only happens when the registry itself is dropped.
    pub fn subscribe_tools_changed(&self) -> broadcast::Receiver<String> {
        self.tools_changed_tx.subscribe()
    }

    /// Emit a synthetic tools-changed tick for `endpoint` on the relay-wide
    /// broadcast. Used by management mutations (e.g. enable/disable an
    /// endpoint or per-endpoint tool) that change the advertised catalog
    /// without a real `notifications/tools/list_changed` event from the
    /// underlying adapter. Sending on a closed broadcast is ignored.
    pub(crate) fn tick_tools_changed(&self, endpoint: &str) {
        let _ = self.tools_changed_tx.send(endpoint.to_string());
    }

    /// Send a synthetic tools-changed tick on the relay-wide broadcast for
    /// the given endpoint name. Test-only escape hatch so server-side
    /// handler tests can exercise the fan-out path without standing up a
    /// real adapter and listener task.
    #[cfg(test)]
    pub(crate) fn tick_tools_changed_for_test(&self, endpoint: &str) {
        self.tick_tools_changed(endpoint);
    }

    /// Register an adapter under the given endpoint name.
    pub async fn register(
        &self,
        name: String,
        adapter: Box<dyn McpAdapter>,
        transport: String,
        description: Option<String>,
        tool_prefix: Option<String>,
    ) {
        debug!(endpoint = %name, ?tool_prefix, "Registering adapter");
        let listener_handle = self.spawn_tools_changed_listener(&name, adapter.as_ref());
        let endpoint_for_tick = name.clone();
        let previous = self.adapters.write().await.insert(
            name,
            RegisteredAdapter {
                adapter,
                transport,
                description,
                tool_prefix,
                last_activity: None,
                disabled: false,
                disabled_tools: HashSet::new(),
                tool_cache: RwLock::new(None),
                tool_cache_populate_lock: Mutex::new(()),
                tools_changed_task: listener_handle,
            },
        );
        if let Some(prev) = previous {
            if let Some(handle) = prev.tools_changed_task {
                handle.abort();
            }
        }
        self.invalidate_catalog_cache().await;
        let _ = self.tools_changed_tx.send(endpoint_for_tick);
    }

    /// Remove an adapter by endpoint name.
    pub async fn remove(&self, name: &str) -> Option<RegisteredAdapter> {
        let result = self.adapters.write().await.remove(name);
        if let Some(entry) = &result {
            if let Some(handle) = &entry.tools_changed_task {
                handle.abort();
            }
            self.invalidate_catalog_cache().await;
            let _ = self.tools_changed_tx.send(name.to_string());
        }
        result
    }

    /// Subscribe to the adapter's tools-changed broadcast (if any) and spawn a
    /// background task that invalidates the per-endpoint and catalog caches on
    /// every tick. Returns the spawned task's [`AbortHandle`], or `None` if the
    /// adapter does not expose a receiver.
    fn spawn_tools_changed_listener(
        &self,
        name: &str,
        adapter: &dyn McpAdapter,
    ) -> Option<AbortHandle> {
        let rx = adapter.subscribe_tools_changed()?;
        let registry = self.clone();
        let endpoint = name.to_string();
        let task = tokio::spawn(async move {
            Self::tools_changed_listener_loop(registry, endpoint, rx).await;
        });
        Some(task.abort_handle())
    }

    /// Listener loop driven by `subscribe_tools_changed`. Each successful tick
    /// — as well as a `Lagged` error (treated as a missed-but-coalesced tick)
    /// — invalidates the per-endpoint tools cache followed by the merged
    /// catalog cache, then fans the tick into the relay-wide
    /// `tools_changed_tx` (carrying the endpoint name) so downstream
    /// consumers (e.g. the `/mcp/sse` handler, or future per-profile SSE
    /// handlers that filter by endpoint membership) can forward
    /// `notifications/tools/list_changed` to connected MCP clients. A
    /// `Closed` receiver terminates the loop.
    async fn tools_changed_listener_loop(
        registry: Self,
        endpoint: String,
        mut rx: broadcast::Receiver<()>,
    ) {
        loop {
            match rx.recv().await {
                Ok(()) => {
                    registry.invalidate_endpoint_tool_cache(&endpoint).await;
                    registry.invalidate_catalog_cache().await;
                    let _ = registry.tools_changed_tx.send(endpoint.clone());
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(
                        endpoint = %endpoint,
                        skipped,
                        "tools-changed listener lagged; treating as tick"
                    );
                    registry.invalidate_endpoint_tool_cache(&endpoint).await;
                    registry.invalidate_catalog_cache().await;
                    let _ = registry.tools_changed_tx.send(endpoint.clone());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!(endpoint = %endpoint, "tools-changed listener channel closed");
                    break;
                }
            }
        }
    }

    /// Abort any existing tools-changed listener for `name` and re-subscribe
    /// against the adapter currently stored at that endpoint. Call this after
    /// swapping `entry.adapter` in place (e.g. background init, restart). No-op
    /// if `name` is not registered.
    pub async fn rewire_tools_changed_listener(&self, name: &str) {
        let new_handle = {
            let entries = self.adapters.read().await;
            match entries.get(name) {
                Some(entry) => self.spawn_tools_changed_listener(name, entry.adapter.as_ref()),
                None => return,
            }
        };
        let mut entries = self.adapters.write().await;
        if let Some(entry) = entries.get_mut(name) {
            if let Some(old) = entry.tools_changed_task.take() {
                old.abort();
            }
            entry.tools_changed_task = new_handle;
        } else if let Some(handle) = new_handle {
            handle.abort();
        }
    }

    /// Invalidate the cached catalog so the next read rebuilds it, and bump
    /// the catalog generation counter so downstream caches (e.g. search
    /// index) can detect the change. Also clears every per-endpoint tools
    /// cache so existing callers don't regress when the underlying adapters
    /// might have changed.
    pub async fn invalidate_catalog_cache(&self) {
        self.invalidate_all_tool_caches().await;
        *self.catalog_cache.write().await = None;
        self.schema_cache.write().await.clear();
        self.catalog_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Invalidate the per-endpoint tools cache for a single adapter, clear
    /// the merged catalog cache, and bump the catalog generation counter.
    /// Used when an adapter is swapped in place (e.g. background init,
    /// restart) so the next read fetches fresh tools from the new adapter.
    pub async fn invalidate_endpoint_tool_cache(&self, name: &str) {
        {
            let entries = self.adapters.read().await;
            if let Some(entry) = entries.get(name) {
                *entry.tool_cache.write().await = None;
            }
        }
        *self.catalog_cache.write().await = None;
        self.catalog_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear every per-endpoint tools cache without touching the merged
    /// catalog cache or the generation counter. Used internally by
    /// [`invalidate_catalog_cache`] and available to callers that want a
    /// surgical sweep across all adapters.
    pub async fn invalidate_all_tool_caches(&self) {
        let entries = self.adapters.read().await;
        for entry in entries.values() {
            *entry.tool_cache.write().await = None;
        }
    }

    /// Return the current catalog generation. Starts at 0; incremented on
    /// every catalog-affecting mutation (register/remove/disable/enable).
    pub fn catalog_generation(&self) -> u64 {
        self.catalog_generation.load(Ordering::Relaxed)
    }

    /// List endpoint names of healthy adapters.
    #[allow(dead_code)] // Kept for future management API use
    pub async fn list_healthy(&self) -> Vec<String> {
        let adapters = self.adapters.read().await;
        adapters
            .iter()
            .filter(|(_, entry)| matches!(entry.adapter.health(), HealthStatus::Healthy))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Deduplicated, lower-cased server types reported by Healthy adapters.
    ///
    /// Walks the adapter map, filters to [`HealthStatus::Healthy`], calls
    /// [`McpAdapter::server_type`], lowercases the result and collects into a
    /// [`BTreeSet`] so the output is sorted. Adapters whose `server_type()`
    /// returns `None` (not yet initialized or transport that does not record
    /// it) are skipped.
    #[allow(dead_code)] // Superseded by `all_server_types`; kept for potential future Healthy-only callers.
    pub async fn ready_server_types(&self) -> BTreeSet<String> {
        let adapters = self.adapters.read().await;
        adapters
            .values()
            .filter(|entry| matches!(entry.adapter.health(), HealthStatus::Healthy))
            .filter_map(|entry| entry.adapter.server_type())
            .map(|s| s.to_lowercase())
            .collect()
    }

    /// Number of currently `Healthy` adapter instances (NOT deduplicated by
    /// server type — three Gmail accounts count as three).
    #[allow(dead_code)] // Superseded by `all_endpoint_count`; kept for potential future Healthy-only callers.
    pub async fn ready_endpoint_count(&self) -> usize {
        let adapters = self.adapters.read().await;
        adapters
            .values()
            .filter(|entry| matches!(entry.adapter.health(), HealthStatus::Healthy))
            .count()
    }

    /// Deduplicated, lower-cased server types across **all** registered
    /// adapters regardless of health.
    ///
    /// For each adapter, prefers the cached upstream-derived `server_type()`
    /// (populated after a successful `initialize` handshake) and falls back
    /// to [`McpAdapter::configured_server_type`] (the sanitized
    /// `server_type_override`) so endpoints with a configured override
    /// surface immediately, before they ever transition to Healthy.
    /// Adapters with neither value are skipped.
    pub async fn all_server_types(&self) -> BTreeSet<String> {
        let adapters = self.adapters.read().await;
        adapters
            .values()
            .filter_map(|entry| {
                entry
                    .adapter
                    .server_type()
                    .or_else(|| entry.adapter.configured_server_type())
            })
            .map(|s| s.to_lowercase())
            .collect()
    }

    /// Total number of registered adapter instances regardless of health
    /// (NOT deduplicated by server type — three Gmail accounts count as
    /// three).
    pub async fn all_endpoint_count(&self) -> usize {
        let adapters = self.adapters.read().await;
        adapters.len()
    }

    /// Profile-scoped variant of [`Self::all_server_types`] — only adapters
    /// whose endpoint name is in `allowed_endpoints` contribute. Used by the
    /// `_for_profile` advertising builders so per-profile `instructions` and
    /// meta-tool descriptions list only the profile's server types.
    pub async fn server_types_in(&self, allowed_endpoints: &HashSet<String>) -> BTreeSet<String> {
        let adapters = self.adapters.read().await;
        adapters
            .iter()
            .filter(|(name, _)| allowed_endpoints.contains(*name))
            .filter_map(|(_, entry)| {
                entry
                    .adapter
                    .server_type()
                    .or_else(|| entry.adapter.configured_server_type())
            })
            .map(|s| s.to_lowercase())
            .collect()
    }

    /// Profile-scoped variant of [`Self::all_endpoint_count`] — counts only
    /// adapters whose endpoint name is in `allowed_endpoints`.
    pub async fn endpoint_count_in(&self, allowed_endpoints: &HashSet<String>) -> usize {
        let adapters = self.adapters.read().await;
        adapters
            .keys()
            .filter(|name| allowed_endpoints.contains(*name))
            .count()
    }

    /// Access the underlying adapters map (for management API use).
    pub fn entries(&self) -> &Arc<RwLock<HashMap<String, RegisteredAdapter>>> {
        &self.adapters
    }

    /// Build merged tool catalog from all adapters, with prefix-aware names.
    ///
    /// Naming strategy:
    /// 1. Each adapter has a `tool_prefix` (from config's `resolved_tool_prefix`).
    /// 2. **Single-server no-prefix mode**: if only one non-disabled adapter is
    ///    registered, tool names are passed through without any prefix.
    /// 3. Otherwise, tools are named `{tool_prefix}__{tool}`.
    ///
    /// Also builds and returns a reverse lookup map for use by `route_tool_call`.
    ///
    /// - Healthy adapters: tools are included with `[endpoint_name]` or `[description]`
    ///   prepended to the tool's description.
    /// - Unhealthy (but not disabled) adapters: tools are included with
    ///   `[⚠️ UNAVAILABLE]` prepended to the description so clients can see them
    ///   but know they are currently unavailable.
    /// - Disabled adapters are excluded entirely.
    pub async fn merged_catalog(&self) -> Vec<ToolInfo> {
        let (catalog, _) = self.merged_catalog_with_lookup().await;
        catalog
    }

    /// Build the merged catalog and a reverse lookup map.
    ///
    /// Returns `(catalog, lookup)` where `lookup` maps each prefixed tool name
    /// to `(endpoint_name, raw_tool_name)`.
    ///
    /// Results are cached; call [`invalidate_catalog_cache`] after mutations
    /// that affect the catalog (register/remove/disable/enable).
    pub async fn merged_catalog_with_lookup(
        &self,
    ) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        // Fast path: return cached value if available
        if let Some(cached) = self.catalog_cache.read().await.as_ref() {
            return cached.clone();
        }
        // Build and cache
        self.refresh_catalog().await
    }

    /// Rebuild the catalog, store it in the cache, and return a clone.
    pub async fn refresh_catalog(&self) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        let result = self.build_catalog().await;
        *self.catalog_cache.write().await = Some(result.clone());
        result
    }

    /// Internal: build the merged catalog from all adapters.
    async fn build_catalog(&self) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        let adapters = self.adapters.read().await;

        // Count non-disabled adapters for single-server no-prefix mode
        let active_count = adapters.values().filter(|e| !e.disabled).count();
        let skip_prefix = active_count <= 1;

        let mut catalog = Vec::new();
        let mut lookup: HashMap<String, (String, String)> = HashMap::new();

        for (endpoint_name, entry) in adapters.iter() {
            if entry.disabled {
                debug!(endpoint = %endpoint_name, "Skipping disabled adapter");
                continue;
            }

            let is_healthy = matches!(entry.adapter.health(), HealthStatus::Healthy);
            let label = entry
                .description
                .as_deref()
                .unwrap_or(endpoint_name.as_str());

            // Determine the prefix to use (if any)
            let effective_prefix = if skip_prefix {
                None
            } else {
                entry.tool_prefix.clone()
            };

            match entry.cached_list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        if entry.disabled_tools.contains(&tool.name) {
                            continue;
                        }
                        let final_name = match &effective_prefix {
                            Some(pfx) => prefix::encode_tool_name(pfx, None, &tool.name),
                            None => tool.name.clone(),
                        };
                        let enriched_description = if is_healthy {
                            match tool.description {
                                Some(desc) => Some(format!("[{}] {}", label, desc)),
                                None => Some(format!("[{}]", label)),
                            }
                        } else {
                            match tool.description {
                                Some(desc) => {
                                    Some(format!("[⚠️ UNAVAILABLE] [{}] {}", label, desc))
                                }
                                None => Some(format!("[⚠️ UNAVAILABLE] [{}]", label)),
                            }
                        };
                        lookup.insert(
                            final_name.clone(),
                            (endpoint_name.clone(), tool.name.clone()),
                        );
                        catalog.push(ToolInfo {
                            name: final_name,
                            description: enriched_description,
                            input_schema: tool.input_schema,
                            annotations: tool.annotations,
                        });
                    }
                }
                Err(e) => {
                    warn!(endpoint = %endpoint_name, error = %e, "Failed to list tools");
                }
            }
        }

        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        (catalog, lookup)
    }

    /// Route a prefixed tool call to the correct adapter.
    ///
    /// Rebuilds the reverse-lookup map from the current catalog to find the
    /// target endpoint and raw tool name for the given prefixed name. Every
    /// early-rejection branch (unknown prefix, missing adapter, disabled
    /// endpoint, unhealthy endpoint, disabled tool) emits a
    /// [`ToolCallEvent::Started`] + [`ToolCallEvent::Failed`] pair on the
    /// shared [`ToolCallEventBus`] (when wired via [`Self::with_event_bus`])
    /// so the desktop overlay shows the same brief in-flight → failed card
    /// it would render for an adapter-side failure.
    pub async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        let (catalog, lookup) = self.merged_catalog_with_lookup().await;

        let (endpoint, tool) = match lookup.get(prefixed_name) {
            Some(v) => v,
            None => {
                // Branch 1: unknown prefixed name. We have no endpoint or
                // adapter to consult, so server_type/server_name are omitted
                // and transport falls back to the "unknown" sentinel.
                return Err(self.publish_early_rejection(
                    prefixed_name.to_string(),
                    "unknown".to_string(),
                    "unknown".to_string(),
                    None,
                    None,
                    format!("no tool found for prefixed name '{}'", prefixed_name),
                ));
            }
        };

        let adapters = self.adapters.read().await;
        let entry = match adapters.get(endpoint) {
            Some(e) => e,
            None => {
                // Branch 2: lookup pointed at an endpoint the registry no
                // longer has an adapter for (e.g. catalog cached across a
                // remove). transport/server_type/server_name unknown.
                return Err(self.publish_early_rejection(
                    tool.clone(),
                    endpoint.clone(),
                    "unknown".to_string(),
                    None,
                    None,
                    format!("no adapter found for endpoint '{}'", endpoint),
                ));
            }
        };

        if entry.disabled {
            // Branch 3: endpoint is administratively disabled.
            return Err(self.publish_early_rejection(
                tool.clone(),
                endpoint.clone(),
                entry.transport.clone(),
                entry.adapter.server_type(),
                entry.adapter.upstream_server_name(),
                format!("endpoint '{}' is disabled", endpoint),
            ));
        }

        if !matches!(entry.adapter.health(), HealthStatus::Healthy) {
            // Branch 4: endpoint is registered + enabled but not Healthy.
            return Err(self.publish_early_rejection(
                tool.clone(),
                endpoint.clone(),
                entry.transport.clone(),
                entry.adapter.server_type(),
                entry.adapter.upstream_server_name(),
                format!(
                    "tool '{}' is currently unavailable: endpoint '{}' is not healthy",
                    tool, endpoint
                ),
            ));
        }

        if entry.disabled_tools.contains(tool) {
            // Branch 5: tool is disabled on this endpoint. Normally
            // `merged_catalog_with_lookup` filters disabled tools so this
            // branch is only reachable when the lookup was cached before
            // the tool was added to `disabled_tools`.
            return Err(self.publish_early_rejection(
                tool.clone(),
                endpoint.clone(),
                entry.transport.clone(),
                entry.adapter.server_type(),
                entry.adapter.upstream_server_name(),
                format!("tool '{}' is disabled on endpoint '{}'", tool, endpoint),
            ));
        }

        // Schema validation. Runs after the disabled-tools check and before
        // adapter dispatch so both the direct (`mcp_tools_call`) and the
        // JS-sandbox paths are covered by this single insertion. Skipped
        // entirely when `relay.validate_inputs` is `false`. Validation
        // failures return `Ok(isError: true)` — the call was understood, the
        // arguments just don't match the schema, so the model can self-correct
        // through the normal tool-result channel rather than a JSON-RPC error.
        if self.validate_inputs.load(Ordering::Relaxed) {
            if let Some(tool_info) = catalog.iter().find(|t| t.name == prefixed_name) {
                if let Some((result, error_message)) = self
                    .validate_arguments(prefixed_name, &tool_info.input_schema, &arguments)
                    .await
                {
                    self.publish_validation_failure(
                        tool.clone(),
                        endpoint.clone(),
                        entry.transport.clone(),
                        entry.adapter.server_type(),
                        entry.adapter.upstream_server_name(),
                        error_message,
                    );
                    return Ok(result);
                }
            }
        }

        info!(
            tool = %tool,
            endpoint = %endpoint,
            prefixed = %prefixed_name,
            "Routing tool call"
        );

        // Observability capture (R4). Inline on the dispatched `tools/call`
        // path. Cheap no-op when the pipeline is unwired or disabled, and
        // skipped for internal callers that have no `request_uid`. Enqueue is
        // non-blocking (`try_send`); overflow drops are counted in the handle.
        let Some(obs) = self.observability.as_ref().filter(|o| o.is_enabled()) else {
            return entry.adapter.call_tool(tool, arguments).await;
        };
        let span_ctx = current_request_context();
        // Gate capture on an inbound request context (skip internal callers),
        // but key the row/payload on a freshly-minted per-call uuid rather than
        // the span's inbound `request_uid`. An `execute_tools` JS batch reuses a
        // single inbound `request_uid` across all its inner `tools/call`s, so
        // reusing it here would collapse them into one row and overwrite each
        // other's payloads. `jsonrpc_id` (recorded below) still groups a batch.
        if span_ctx.request_uid.is_none() {
            return entry.adapter.call_tool(tool, arguments).await;
        }
        let request_uid = uuid::Uuid::new_v4().to_string();

        let store_payloads = obs.store_payloads();
        let request_value = arguments.clone();
        let request_bytes = request_value.to_string().len() as i64;
        let server_type = entry.adapter.server_type();
        let server_name = entry.adapter.upstream_server_name();
        let transport = entry.transport.clone();
        let endpoint_name = endpoint.clone();
        let tool_name = tool.clone();
        let (client_name, client_version, client_user_agent, client_origin) = match &span_ctx.client
        {
            Some(c) => (
                c.name.clone(),
                c.version.clone(),
                c.user_agent.clone(),
                c.origin.clone(),
            ),
            None => (None, None, None, None),
        };

        let ts_start = chrono::Utc::now().timestamp_millis();
        let started = Instant::now();
        let result = entry.adapter.call_tool(tool, arguments).await;
        let duration_ms = started.elapsed().as_millis() as i64;
        let ts_end = ts_start + duration_ms;

        let (success, error_message, response_value, response_bytes) = match &result {
            Ok(v) => {
                let bytes = v.to_string().len() as i64;
                // A transport-level `Ok` can still carry a tool-level error
                // envelope (`{ content: [...], isError: true }`). Record it as a
                // failed call while preserving the response payload/bytes.
                match tool_result_error_message(v) {
                    Some(msg) => (false, Some(msg), v.clone(), bytes),
                    None => (true, None, v.clone(), bytes),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                (
                    false,
                    Some(msg.clone()),
                    serde_json::json!({ "error": msg }),
                    0,
                )
            }
        };

        let record = CallRecord {
            id: None,
            request_uid: Some(request_uid),
            jsonrpc_id: span_ctx.jsonrpc_id.clone(),
            endpoint: Some(endpoint_name),
            server_name,
            server_type,
            transport: Some(transport),
            profile: span_ctx.profile.clone(),
            client_name,
            client_version,
            client_user_agent,
            client_origin,
            tool: tool_name,
            ts_start,
            ts_end,
            duration_ms,
            success,
            error_message,
            request_bytes,
            response_bytes,
            streamed: false,
        };
        let payloads = store_payloads.then(|| CapturedPayloads {
            request: request_value,
            response: response_value,
            streamed: false,
        });
        obs.capture(CaptureRecord { record, payloads });

        result
    }

    /// Publish a `Started` + `Failed` pair on the shared
    /// [`ToolCallEventBus`] (when wired) for an early-rejection branch of
    /// [`Self::route_tool_call`], then return the matching
    /// [`AdapterError::ProtocolError`]. Kept in lockstep with the
    /// per-adapter `call_tool` emission pattern in
    /// `adapter/stdio.rs::call_tool` so the overlay sees a uniform
    /// `Started` → `Failed` transition regardless of whether the call was
    /// rejected pre-adapter or returned an error from the upstream MCP
    /// server. The error text returned here MUST match the pre-event
    /// strings (`no tool found for prefixed name`,
    /// `no adapter found for endpoint`, `endpoint '{}' is disabled`,
    /// `tool '{}' is currently unavailable: endpoint '{}' is not healthy`,
    /// `tool '{}' is disabled on endpoint '{}'`) — external log scrapers
    /// may depend on the exact wording.
    fn publish_early_rejection(
        &self,
        tool: String,
        endpoint: String,
        transport: String,
        server_type: Option<String>,
        server_name: Option<String>,
        error_message: String,
    ) -> AdapterError {
        if let Some(bus) = &self.event_bus {
            let span_ctx = current_request_context();
            let request_id = uuid::Uuid::new_v4().to_string();
            // Emit `Started` first so the overlay can spawn an in-flight
            // card before it sees `Failed`, matching the
            // adapter-side ordering.
            bus.send(ToolCallEvent::Started {
                request_id: request_id.clone(),
                jsonrpc_id: span_ctx.jsonrpc_id.clone(),
                request_uid: span_ctx.request_uid.clone(),
                ts: iso8601_now(),
                endpoint,
                transport,
                server_type,
                server_name,
                profile: span_ctx.profile.clone(),
                tool,
                annotations: None,
                client: span_ctx.client.clone(),
            });
            bus.send(ToolCallEvent::Failed {
                request_id,
                jsonrpc_id: span_ctx.jsonrpc_id,
                ts: iso8601_now(),
                duration_ms: 0,
                status: "error".to_string(),
                error_message: error_message.clone(),
            });
        }
        AdapterError::ProtocolError(error_message)
    }

    /// Publish a `Started` + `Failed` pair on the shared
    /// [`ToolCallEventBus`] (when wired) for an input-validation failure in
    /// [`Self::route_tool_call`]. Mirrors [`Self::publish_early_rejection`]
    /// but tags the `Failed` event with `status: "validation_error"` so the
    /// overlay can distinguish schema failures from upstream failures, and
    /// returns nothing — the caller returns the `isError: true` tool result
    /// to the client rather than an [`AdapterError`].
    fn publish_validation_failure(
        &self,
        tool: String,
        endpoint: String,
        transport: String,
        server_type: Option<String>,
        server_name: Option<String>,
        error_message: String,
    ) {
        if let Some(bus) = &self.event_bus {
            let span_ctx = current_request_context();
            let request_id = uuid::Uuid::new_v4().to_string();
            // Emit `Started` first so the overlay spawns an in-flight card
            // before it sees `Failed`, matching the adapter-side ordering.
            bus.send(ToolCallEvent::Started {
                request_id: request_id.clone(),
                jsonrpc_id: span_ctx.jsonrpc_id.clone(),
                request_uid: span_ctx.request_uid.clone(),
                ts: iso8601_now(),
                endpoint,
                transport,
                server_type,
                server_name,
                profile: span_ctx.profile.clone(),
                tool,
                annotations: None,
                client: span_ctx.client.clone(),
            });
            bus.send(ToolCallEvent::Failed {
                request_id,
                jsonrpc_id: span_ctx.jsonrpc_id,
                ts: iso8601_now(),
                duration_ms: 0,
                status: "validation_error".to_string(),
                error_message,
            });
        }
    }

    /// Validate `arguments` against the tool's compiled `inputSchema`.
    ///
    /// Returns `None` when the call should pass through — either the schema
    /// is degenerate/malformed (no compiled validator cached) or the
    /// arguments are valid. Returns `Some((result, message))` when validation
    /// fails: `result` is the MCP `isError: true` tool result to return to the
    /// client and `message` is the formatted error text for the event bus.
    ///
    /// Compilation is lazy and cached per prefixed tool name — see
    /// [`Self::compiled_validator`].
    async fn validate_arguments(
        &self,
        prefixed_name: &str,
        schema: &serde_json::Value,
        arguments: &serde_json::Value,
    ) -> Option<(serde_json::Value, String)> {
        let validator = self.compiled_validator(prefixed_name, schema).await?;
        validate_with_validator(prefixed_name, &validator, schema, arguments)
    }

    /// Return the compiled validator for `prefixed_name`, compiling and
    /// caching it on first use. `None` means there is no useful schema to
    /// validate against (degenerate schema or a schema that failed to
    /// compile); the cache stores `None` in that case so compilation is not
    /// retried until the catalog is invalidated.
    async fn compiled_validator(
        &self,
        prefixed_name: &str,
        schema: &serde_json::Value,
    ) -> Option<Arc<Validator>> {
        // Fast path: read-locked cache hit.
        {
            let cache = self.schema_cache.read().await;
            if let Some(entry) = cache.get(prefixed_name) {
                return entry.clone();
            }
        }
        // Slow path: take the write lock and re-check so concurrent callers
        // don't compile the same schema twice.
        let mut cache = self.schema_cache.write().await;
        if let Some(entry) = cache.get(prefixed_name) {
            return entry.clone();
        }
        let compiled = compile_input_schema(prefixed_name, schema);
        cache.insert(prefixed_name.to_string(), compiled.clone());
        compiled
    }
}

/// Decide whether a tool's `inputSchema` is worth compiling for validation.
///
/// Mirrors the original `validate_tool_args` bypass rules (spec §3.3): the
/// schema must describe an object (`type` is `"object"` or absent) and carry
/// at least one real constraint. A bare `{"type": "object"}` has nothing to
/// validate and is skipped (pass-through), but anything that meaningfully
/// constrains the arguments is compiled and enforced. Constraints can be
/// expressed as object shape (`properties`/`required`), a closed object
/// (`additionalProperties`/`unevaluatedProperties` set to `false` or a
/// subschema), references/combinators (`$ref`, `allOf`, `anyOf`, `oneOf`,
/// `not`, `if`), or other object-shape keywords (`patternProperties`,
/// `propertyNames`, `dependentRequired`, `dependentSchemas`, `dependencies`,
/// `minProperties`, `maxProperties`, `enum`, `const`). Recognizing these (and
/// not just `properties`/`required`) ensures schemas that constrain solely via
/// `$ref`/combinators/object keywords are validated instead of passed through.
fn schema_is_validatable(schema: &serde_json::Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    if let Some(t) = obj.get("type") {
        if t.as_str() != Some("object") {
            return false;
        }
    }
    obj.iter()
        .any(|(key, value)| schema_keyword_constrains(key, value))
}

/// Whether a single top-level schema keyword (with a non-trivial value)
/// meaningfully constrains the instance, used by [`schema_is_validatable`].
fn schema_keyword_constrains(key: &str, value: &serde_json::Value) -> bool {
    match key {
        // Object shape / key-restricting keywords whose value is a map.
        "properties" | "patternProperties" | "dependentRequired" | "dependentSchemas"
        | "dependencies" => value.as_object().map(|o| !o.is_empty()).unwrap_or(false),
        // Keywords whose value is a (non-empty) array.
        "required" | "allOf" | "anyOf" | "oneOf" | "enum" => {
            value.as_array().map(|a| !a.is_empty()).unwrap_or(false)
        }
        // A closed object: `false` or a subschema constrains, plain `true` does not.
        "additionalProperties" | "unevaluatedProperties" => {
            !matches!(value, serde_json::Value::Bool(true))
        }
        // `minProperties: 0` is a no-op; any positive bound constrains.
        "minProperties" => value.as_u64().map(|n| n > 0).unwrap_or(false),
        "maxProperties" => value.is_number(),
        // A `$ref` defers to another (presumably constraining) schema.
        "$ref" => value.is_string(),
        // Presence alone constrains (subschemas / conditional / fixed value).
        "not" | "if" | "propertyNames" | "const" => true,
        _ => false,
    }
}

/// Compile a tool's `inputSchema` into a reusable [`Validator`], or return
/// `None` to signal pass-through. Degenerate schemas (see
/// [`schema_is_validatable`]) and schemas the upstream server shipped
/// malformed both yield `None` — the relay never blocks a call because of a
/// bad upstream schema; it lets the upstream handle it. `format` is enforced
/// as an assertion (spec §11.1).
fn compile_input_schema(prefixed_name: &str, schema: &serde_json::Value) -> Option<Arc<Validator>> {
    if !schema_is_validatable(schema) {
        return None;
    }
    match jsonschema::options()
        .should_validate_formats(true)
        .build(schema)
    {
        Ok(validator) => {
            debug!(tool = %prefixed_name, "Compiled input schema for validation");
            Some(Arc::new(validator))
        }
        Err(e) => {
            warn!(
                tool = %prefixed_name,
                error = %e,
                "Failed to compile input schema — validation will be skipped for this tool"
            );
            None
        }
    }
}

/// Upper bound on a captured tool-level `error_message`, in characters. Tool
/// error envelopes are usually short, but a misbehaving server could return a
/// large blob; cap it so the durable row stays bounded.
const MAX_CAPTURED_ERROR_MESSAGE_CHARS: usize = 2048;

/// Inspect a transport-level `Ok` `tools/call` result for a tool-level error
/// envelope (`{ content: [...], isError: true }`). Returns the captured
/// `error_message` (the first `content[].text` string, truncated) when the tool
/// reported an error, or `None` for a normal success (`isError` absent/false).
fn tool_result_error_message(value: &serde_json::Value) -> Option<String> {
    if value.get("isError").and_then(|e| e.as_bool()) != Some(true) {
        return None;
    }
    let text = value
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("text").and_then(|t| t.as_str()))
        })
        .unwrap_or("tool call returned an error result");
    let truncated: String = text
        .chars()
        .take(MAX_CAPTURED_ERROR_MESSAGE_CHARS)
        .collect();
    Some(truncated)
}

/// Format the collected validation errors into the model-friendly MCP error
/// text (spec §5.5). All errors from a single pass are listed together so the
/// model can correct every field in one retry.
fn format_validation_message(prefixed_name: &str, errors: &[String]) -> String {
    let mut message = format!("Input validation failed for tool '{}':\n\n", prefixed_name);
    for error in errors {
        message.push_str("• ");
        message.push_str(error);
        message.push('\n');
    }
    message.push_str("\nPlease correct these fields and try again.");
    message
}

/// Validate `arguments` against an already-compiled [`Validator`], formatting
/// any failures with the same per-kind message table the registry's per-tool
/// path uses. `schema` is the validator's source schema, consulted to
/// enumerate known parameter names for `additionalProperties` enrichment.
///
/// Returns `Some((result, message))` — the `isError: true` MCP tool result to
/// return to the client and the formatted error text for logs/event bus — when
/// validation fails, or `None` when the arguments are valid.
///
/// `AdapterRegistry::validate_arguments` calls this after a lazy cache lookup;
/// the meta-tool path in `server.rs` calls it with schemas precompiled at
/// startup, so both surfaces emit identical error text.
pub(crate) fn validate_with_validator(
    name: &str,
    validator: &Validator,
    schema: &serde_json::Value,
    arguments: &serde_json::Value,
) -> Option<(serde_json::Value, String)> {
    let mut errors: Vec<String> = validator
        .iter_errors(arguments)
        .map(|e| format_validation_error(&e, schema))
        .collect();
    if errors.is_empty() {
        return None;
    }
    // Some schema shapes emit one error per offending value with identical text
    // (e.g. `additionalProperties: false` with no declared `properties`, where
    // each extra key yields the same "accepts no parameters" message). Collapse
    // exact duplicates, preserving order, so the model sees each point once.
    let mut seen = std::collections::HashSet::new();
    errors.retain(|message| seen.insert(message.clone()));
    warn!(
        tool = %name,
        error_count = errors.len(),
        "Input validation failed — returning structured error to client"
    );
    for message in &errors {
        debug!(tool = %name, "Validation error: {}", message);
    }
    let text = format_validation_message(name, &errors);
    let result = serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    });
    Some((result, text))
}

/// Maximum edit distance for a "did you mean …?" suggestion (spec §5.4).
const SUGGESTION_MAX_DISTANCE: usize = 2;

/// Format a single [`ValidationError`] into a model-friendly bullet line per the
/// spec §5.2 kind→message table, §5.3 path rendering, and §5.4 suggestions.
///
/// `schema` is the tool's `inputSchema`, used to enumerate the known property
/// names for `additionalProperties` errors (the error kind only carries the
/// *unexpected* names) and for close-match suggestions. Any kind the table does
/// not cover falls through to the crate's default `ValidationError::to_string()`.
fn format_validation_error(error: &ValidationError, schema: &Value) -> String {
    let path = render_instance_path(error.instance_path());
    let instance = error.instance();
    match error.kind() {
        ValidationErrorKind::Required { property } => {
            let name = property.as_str().unwrap_or_default();
            let field = join_field(&path, name);
            format!("Field '{}': required field is missing.", field)
        }
        ValidationErrorKind::Type { kind } => {
            let expected = type_kind_label(kind);
            let actual = json_type_name(instance);
            let mut msg = format!(
                "Field '{}': expected {}, got {}.",
                field_label(&path),
                expected,
                actual
            );
            if expected == "array" {
                if let Some(s) = instance.as_str() {
                    if s.contains(',') {
                        msg.push_str(&format!(
                            " Did you mean {}?",
                            comma_string_to_array_suggestion(s)
                        ));
                    }
                }
            }
            msg
        }
        ValidationErrorKind::Enum { options } => {
            let variants = enum_variant_labels(options);
            let mut msg = format!(
                "Field '{}': value {} is not allowed. Allowed values: {}.",
                field_label(&path),
                render_instance_value(instance),
                variants.join(", ")
            );
            if let Some(s) = instance.as_str() {
                if let Some(best) = closest_match(s, &enum_string_variants(options)) {
                    msg.push_str(&format!(" Did you mean \"{}\"?", best));
                }
            }
            msg
        }
        ValidationErrorKind::AdditionalProperties { unexpected }
        | ValidationErrorKind::UnevaluatedProperties { unexpected } => {
            // The error sits on the object that carries the unexpected keys,
            // which may be a nested object rather than the root. Resolve the
            // subschema at that instance path so the "Valid parameters" list
            // names the keys that object actually allows.
            let subschema = subschema_at_instance_path(schema, error.instance_path());
            let known = schema_property_names(subschema);
            // The crate batches every unexpected key into one error, so name
            // them all (sorted for deterministic output) rather than just the
            // first — otherwise the model fixes one and is rejected for the
            // next on each retry.
            let mut keys: Vec<&str> = unexpected.iter().map(String::as_str).collect();
            keys.sort_unstable();
            let fields: Vec<String> = keys.iter().map(|key| join_field(&path, key)).collect();
            let mut msg = if fields.len() == 1 {
                format!(
                    "Field '{}': unknown parameter. Valid parameters: {}.",
                    fields[0],
                    known.join(", ")
                )
            } else {
                let listed = fields
                    .iter()
                    .map(|field| format!("'{}'", field))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Fields {}: unknown parameters. Valid parameters: {}.",
                    listed,
                    known.join(", ")
                )
            };
            if fields.len() == 1 {
                if let Some(best) = closest_match(keys[0], &known) {
                    msg.push_str(&format!(" Did you mean '{}'?", best));
                }
            } else {
                for (key, field) in keys.iter().zip(fields.iter()) {
                    if let Some(best) = closest_match(key, &known) {
                        msg.push_str(&format!(" Did you mean '{}' for '{}'?", best, field));
                    }
                }
            }
            msg
        }
        ValidationErrorKind::MinLength { limit } => format!(
            "Field '{}': string length {} is outside range; minimum length is {}.",
            field_label(&path),
            string_len(instance),
            limit
        ),
        ValidationErrorKind::MaxLength { limit } => format!(
            "Field '{}': string length {} is outside range; maximum length is {}.",
            field_label(&path),
            string_len(instance),
            limit
        ),
        ValidationErrorKind::Minimum { limit } => format!(
            "Field '{}': value {} is outside range; minimum is {}.",
            field_label(&path),
            render_instance_value(instance),
            limit
        ),
        ValidationErrorKind::Maximum { limit } => format!(
            "Field '{}': value {} is outside range; maximum is {}.",
            field_label(&path),
            render_instance_value(instance),
            limit
        ),
        ValidationErrorKind::ExclusiveMinimum { limit } => format!(
            "Field '{}': value {} is outside range; must be greater than {}.",
            field_label(&path),
            render_instance_value(instance),
            limit
        ),
        ValidationErrorKind::ExclusiveMaximum { limit } => format!(
            "Field '{}': value {} is outside range; must be less than {}.",
            field_label(&path),
            render_instance_value(instance),
            limit
        ),
        ValidationErrorKind::Pattern { pattern } => format!(
            "Field '{}': value {} does not match pattern {}.",
            field_label(&path),
            render_instance_value(instance),
            pattern
        ),
        ValidationErrorKind::Format { format } => format!(
            "Field '{}': value {} is not a valid {}.",
            field_label(&path),
            render_instance_value(instance),
            format
        ),
        ValidationErrorKind::MinItems { limit } => format!(
            "Field '{}': array length {} is outside range; minimum is {}.",
            field_label(&path),
            array_len(instance),
            limit
        ),
        ValidationErrorKind::MaxItems { limit } => format!(
            "Field '{}': array length {} is outside range; maximum is {}.",
            field_label(&path),
            array_len(instance),
            limit
        ),
        ValidationErrorKind::UniqueItems => format!(
            "Field '{}': array contains duplicate items.",
            field_label(&path)
        ),
        ValidationErrorKind::FalseSchema => {
            // Reached when a `false` subschema rejects a value — most commonly
            // `additionalProperties: false` on a schema with no declared
            // `properties`. The crate emits one such error per extra key with
            // an empty path and only the *value* as the instance, so the key
            // name can't be recovered here. Such an object accepts nothing, so
            // the actionable fix is simply to remove the arguments.
            let segments: Vec<LocationSegment> = error.instance_path().iter().collect();
            let known = schema_property_names(subschema_for_segments(schema, &segments));
            if known.is_empty() {
                if path.is_empty() {
                    "Field '<root>': this tool accepts no parameters; remove all arguments and try again."
                        .to_string()
                } else {
                    format!(
                        "Field '{}': accepts no parameters; remove its arguments and try again.",
                        path
                    )
                }
            } else {
                format!(
                    "Field '{}': contains an unknown parameter. Valid parameters: {}.",
                    field_label(&path),
                    known.join(", ")
                )
            }
        }
        // Anything not in the §5.2 table falls through to the crate's default
        // rendering so no error is ever dropped.
        _ => format!("Field '{}': {}", field_label(&path), error),
    }
}

/// Render a [`Location`] (JSON Pointer) as dot/bracket notation for readability
/// (spec §5.3): `/assignees/0` → `assignees[0]`, `/nested/config` →
/// `nested.config`, root → empty string.
fn render_instance_path(location: &Location) -> String {
    let mut out = String::new();
    for segment in location.iter() {
        match segment {
            LocationSegment::Property(name) => {
                if out.is_empty() {
                    out.push_str(&name);
                } else {
                    out.push('.');
                    out.push_str(&name);
                }
            }
            LocationSegment::Index(index) => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
        }
    }
    out
}

/// Display label for a field path, falling back to `<root>` when the error sits
/// on the instance root and no property name is available.
fn field_label(path: &str) -> &str {
    if path.is_empty() {
        "<root>"
    } else {
        path
    }
}

/// Join a base path and a leaf property name with dot notation (spec §5.3),
/// handling the root case where the base is empty.
fn join_field(base: &str, leaf: &str) -> String {
    if base.is_empty() {
        leaf.to_string()
    } else if leaf.is_empty() {
        base.to_string()
    } else {
        format!("{}.{}", base, leaf)
    }
}

/// Human-readable label for an expected [`TypeKind`].
fn type_kind_label(kind: &TypeKind) -> String {
    match kind {
        TypeKind::Single(ty) => ty.to_string(),
        TypeKind::Multiple(set) => set
            .iter()
            .map(|ty| ty.to_string())
            .collect::<Vec<_>>()
            .join(" or "),
    }
}

/// Human-readable label for the actual JSON type of `value`.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_f64() {
                "number"
            } else {
                "integer"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Render an instance value for inclusion in a message: strings are quoted,
/// everything else uses its compact JSON form.
fn render_instance_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
        other => other.to_string(),
    }
}

/// Character length of a string instance (0 for non-strings).
fn string_len(value: &Value) -> usize {
    value.as_str().map(|s| s.chars().count()).unwrap_or(0)
}

/// Element count of an array instance (0 for non-arrays).
fn array_len(value: &Value) -> usize {
    value.as_array().map(Vec::len).unwrap_or(0)
}

/// Property names declared by a schema's `properties` map (sorted by serde_json's
/// default object ordering), used for `additionalProperties` valid-parameter
/// lists and close-match suggestions.
fn schema_property_names(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

/// Resolve the subschema that applies to the instance at `location` by walking
/// the root schema's `properties`/`items` along the path. Used so an
/// `additionalProperties` error on a nested object reports that object's valid
/// keys rather than the root's. Falls back to the root schema whenever a
/// segment can't be resolved (e.g. `$ref`, combinator subschemas, or tuple
/// `items` arrays), keeping the message best-effort rather than wrong.
fn subschema_at_instance_path<'a>(root: &'a Value, location: &Location) -> &'a Value {
    let segments: Vec<LocationSegment> = location.iter().collect();
    subschema_for_segments(root, &segments)
}

/// Walk `root`'s `properties`/`items` along `segments`, returning the resolved
/// subschema or falling back to `root` when a segment can't be followed. Shared
/// by [`subschema_at_instance_path`] and the `FalseSchema` arm, which walks to
/// the parent object to enumerate its valid keys.
fn subschema_for_segments<'a>(root: &'a Value, segments: &[LocationSegment]) -> &'a Value {
    let mut current = root;
    for segment in segments {
        let next = match segment {
            LocationSegment::Property(name) => current
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|props| props.get(&**name)),
            LocationSegment::Index(_) => current.get("items").filter(|items| items.is_object()),
        };
        match next {
            Some(schema) => current = schema,
            None => return root,
        }
    }
    current
}

/// Render enum options for the "Allowed values" list: strings are shown raw
/// (unquoted), other JSON values use their compact form.
fn enum_variant_labels(options: &Value) -> Vec<String> {
    options
        .as_array()
        .map(|variants| {
            variants
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// String-typed enum options only, used for close-match suggestions.
fn enum_string_variants(options: &Value) -> Vec<String> {
    options
        .as_array()
        .map(|variants| {
            variants
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert a comma-containing string into a suggested JSON array literal
/// (spec §5.4): `"bug,urgent"` → `["bug", "urgent"]`.
fn comma_string_to_array_suggestion(value: &str) -> String {
    let parts: Vec<String> = value
        .split(',')
        .map(|part| format!("\"{}\"", part.trim()))
        .collect();
    format!("[{}]", parts.join(", "))
}

/// Closest candidate to `input` within [`SUGGESTION_MAX_DISTANCE`] OSA edit
/// distance (spec §5.4), or `None` when nothing is close enough. Ties break on
/// the candidate name for determinism, mirroring the search_tools fuzzy matcher.
fn closest_match<'a>(input: &str, candidates: &'a [String]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|candidate| (strsim::osa_distance(input, candidate), candidate))
        .filter(|(distance, _)| *distance <= SUGGESTION_MAX_DISTANCE)
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)))
        .map(|(_, candidate)| candidate.as_str())
}

/// Abstraction over the catalog/routing surface used by [`MetaToolHandler`]
/// and the JS sandbox, so they can operate on either the global
/// [`AdapterRegistry`] or a per-profile filtered view
/// (`profile_registry::ProfileRegistryView`).
///
/// Per locked decision Relay #2 the surface is intentionally narrow:
/// `merged_catalog`, `merged_catalog_with_lookup`, `route_tool_call`, and
/// `subscribe_tools_changed`. `catalog_generation` is also included because
/// `MetaToolHandler::search_tools` uses it as a cache invalidation key — a
/// per-profile view delegates to the underlying registry's counter so
/// catalog mutations correctly invalidate per-profile caches too.
#[async_trait]
pub trait MetaToolRegistry: Send + Sync {
    /// Build (or fetch the cached) merged tool catalog visible to this
    /// scope. Names are prefixed per the adapter's `tool_prefix` (see
    /// [`AdapterRegistry::merged_catalog`]); profile-scoped impls filter to
    /// tools owned by in-profile endpoints.
    async fn merged_catalog(&self) -> Vec<ToolInfo>;

    /// Variant of [`Self::merged_catalog`] that also returns the
    /// `prefixed_name → (endpoint, raw_name)` reverse lookup map, filtered
    /// to the same scope as the catalog.
    ///
    /// `allow(dead_code)`: callers in the bin compilation unit reach
    /// this method through `MetaToolHandler` which is built only by
    /// `ProfileRegistry::rebuild` and `main.rs` — the bin's dead-code
    /// pass doesn't see those call sites uniformly, so this and
    /// [`Self::subscribe_tools_changed`] are annotated explicitly.
    #[allow(dead_code)]
    async fn merged_catalog_with_lookup(
        &self,
    ) -> (Vec<ToolInfo>, HashMap<String, (String, String)>);

    /// Route a prefixed tool call. Profile-scoped impls reject tools whose
    /// owning endpoint is not in the profile.
    async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: Value,
    ) -> Result<Value, AdapterError>;

    /// Current catalog generation. Monotonically increases on every
    /// catalog-affecting mutation; profile-scoped impls delegate to the
    /// underlying registry's counter.
    fn catalog_generation(&self) -> u64;

    /// Subscribe to the relay-wide tools-changed broadcast. Each tick
    /// yields the endpoint name that emitted the change. Profile-scoped
    /// impls delegate to the underlying registry — consumers (e.g. the
    /// SSE handler in R3.D) filter on endpoint membership.
    #[allow(dead_code)]
    fn subscribe_tools_changed(&self) -> broadcast::Receiver<String>;
}

#[async_trait]
impl MetaToolRegistry for AdapterRegistry {
    async fn merged_catalog(&self) -> Vec<ToolInfo> {
        AdapterRegistry::merged_catalog(self).await
    }

    async fn merged_catalog_with_lookup(
        &self,
    ) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        AdapterRegistry::merged_catalog_with_lookup(self).await
    }

    async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: Value,
    ) -> Result<Value, AdapterError> {
        AdapterRegistry::route_tool_call(self, prefixed_name, arguments).await
    }

    fn catalog_generation(&self) -> u64 {
        AdapterRegistry::catalog_generation(self)
    }

    fn subscribe_tools_changed(&self) -> broadcast::Receiver<String> {
        AdapterRegistry::subscribe_tools_changed(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::StartingAdapter;
    use async_trait::async_trait;
    use serde_json::json;

    #[test]
    fn tool_result_iserror_envelope_is_captured_as_failure() {
        // A transport-level `Ok` carrying a tool-level error envelope must be
        // recorded as a failed call with a non-empty error_message taken from
        // the first text item in `content`.
        let envelope = json!({
            "content": [{ "type": "text", "text": "invalid_grant" }],
            "isError": true,
        });
        let msg =
            tool_result_error_message(&envelope).expect("isError envelope captured as failure");
        assert_eq!(msg, "invalid_grant");
    }

    #[test]
    fn tool_result_success_envelope_is_not_a_failure() {
        // `isError` absent → success; explicit `isError: false` → success.
        assert!(tool_result_error_message(&json!({ "content": [] })).is_none());
        assert!(tool_result_error_message(&json!({
            "content": [{ "type": "text", "text": "ok" }],
            "isError": false,
        }))
        .is_none());
    }

    #[test]
    fn tool_result_iserror_without_text_still_yields_message() {
        // isError with no usable text content still produces a non-empty
        // error_message so the durable row reflects the failure.
        let msg = tool_result_error_message(&json!({ "isError": true, "content": [] }))
            .expect("isError envelope captured as failure");
        assert!(!msg.is_empty());
    }

    #[test]
    fn tool_result_error_message_is_truncated() {
        let long = "x".repeat(MAX_CAPTURED_ERROR_MESSAGE_CHARS + 500);
        let msg = tool_result_error_message(&json!({
            "isError": true,
            "content": [{ "type": "text", "text": long }],
        }))
        .expect("isError envelope captured as failure");
        assert_eq!(msg.chars().count(), MAX_CAPTURED_ERROR_MESSAGE_CHARS);
    }

    /// Regression: an `execute_tools` JS batch reuses one inbound
    /// `request_uid` across all its inner `tools/call`s. The observability
    /// capture block must mint a fresh per-call uuid for each captured row so
    /// the inner calls produce distinct rows (and distinct payload entries when
    /// `store_payloads` is on) instead of collapsing into one row that
    /// overwrites the others, while `jsonrpc_id` still groups the batch.
    #[tokio::test]
    async fn batched_inner_calls_get_distinct_request_uids() {
        use crate::config::ObservabilityConfig;
        use crate::events::SpanFieldCaptureLayer;
        use crate::observability::payloads::PayloadStore;
        use crate::observability::store::{QueryFilter, Store};
        use tracing::Instrument;
        use tracing_subscriber::prelude::*;

        let subscriber = tracing_subscriber::registry().with(SpanFieldCaptureLayer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let store = Arc::new(Store::open_in_memory().unwrap());
        let payloads = Arc::new(PayloadStore::new(10, 128, 256 * 1024));
        let config = ObservabilityConfig {
            enabled: true,
            store_payloads: true,
            ..ObservabilityConfig::default()
        };
        let obs = Observability::new(&config, Arc::clone(&store), Arc::clone(&payloads));

        let registry = AdapterRegistry::new().with_observability(obs);
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("echo")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Both inner calls run inside ONE inbound `request` span (same
        // `request_uid`), exactly as `execute_tools` dispatches a JS batch.
        let shared_uid = "inbound-uid-shared";
        let jsonrpc_id = "42".to_string();
        async {
            registry
                .route_tool_call("echo", json!({ "n": 1 }))
                .await
                .unwrap();
            registry
                .route_tool_call("echo", json!({ "n": 2 }))
                .await
                .unwrap();
        }
        .instrument(tracing::info_span!(
            "request",
            method = "tools/call",
            id = %jsonrpc_id,
            request_uid = %shared_uid,
        ))
        .await;

        // Drain: poll until both rows land via the background consumer.
        let mut rows = Vec::new();
        for _ in 0..50 {
            rows = store.query(&QueryFilter::default(), 10, 0).unwrap();
            if rows.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(rows.len(), 2, "both inner calls captured as separate rows");

        let uids: Vec<String> = rows
            .iter()
            .map(|r| r.request_uid.clone().expect("request_uid set"))
            .collect();
        assert_ne!(uids[0], uids[1], "rows have distinct request_uids");
        assert!(
            uids.iter().all(|u| u != shared_uid),
            "row uids are minted per-call, not the shared inbound request_uid"
        );

        // `jsonrpc_id` is unchanged, so it still groups the batch together.
        assert!(rows.iter().all(|r| r.jsonrpc_id.as_deref() == Some("42")));

        // Each row's payload resolves by its own minted uid and is not
        // overwritten by the sibling call.
        let p0 = payloads.get(&uids[0]).expect("payload for row 0");
        let p1 = payloads.get(&uids[1]).expect("payload for row 1");
        assert_ne!(
            p0.request, p1.request,
            "inner-call payloads not overwritten"
        );
    }

    /// A mock adapter for testing.
    struct MockAdapter {
        health: HealthStatus,
        tools: Vec<ToolInfo>,
        server_type_val: Option<String>,
    }

    impl MockAdapter {
        fn healthy(tools: Vec<ToolInfo>) -> Self {
            Self {
                health: HealthStatus::Healthy,
                tools,
                server_type_val: None,
            }
        }

        #[allow(dead_code)]
        fn healthy_with_type(tools: Vec<ToolInfo>, st: &str) -> Self {
            Self {
                health: HealthStatus::Healthy,
                tools,
                server_type_val: Some(st.to_string()),
            }
        }

        fn unhealthy() -> Self {
            Self {
                health: HealthStatus::Unhealthy("test".into()),
                tools: vec![],
                server_type_val: None,
            }
        }

        fn unhealthy_with_tools(tools: Vec<ToolInfo>) -> Self {
            Self {
                health: HealthStatus::Unhealthy("test".into()),
                tools,
                server_type_val: None,
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
        async fn call_tool(
            &self,
            name: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        fn health(&self) -> HealthStatus {
            self.health.clone()
        }
        fn server_type(&self) -> Option<String> {
            self.server_type_val.clone()
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    fn make_tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{} tool", name)),
            input_schema: json!({"type": "object"}),
            annotations: None,
        }
    }

    // --- Single-server no-prefix mode ---

    #[tokio::test]
    async fn test_single_server_no_prefix() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep1".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep1".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        // Single adapter → no prefix
        assert_eq!(catalog[0].name, "read");
    }

    // --- Multi-server with tool_prefix ---

    #[tokio::test]
    async fn test_multi_server_uses_tool_prefix() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep1".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep1".into()),
            )
            .await;
        registry
            .register(
                "ep2".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("write")])),
                "stdio".into(),
                None,
                Some("ep2".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"ep1__read"));
        assert!(names.contains(&"ep2__write"));
    }

    // --- Unhealthy endpoints still show in catalog ---

    #[tokio::test]
    async fn test_unhealthy_included_in_catalog() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "good".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("tool")])),
                "stdio".into(),
                None,
                Some("good".into()),
            )
            .await;
        registry
            .register(
                "bad".into(),
                Box::new(MockAdapter::unhealthy()),
                "stdio".into(),
                None,
                Some("bad".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        // unhealthy with no tools → only 1 tool in catalog
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "good__tool");
    }

    // --- Route tool call ---

    #[tokio::test]
    async fn test_route_tool_call_single_server() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("echo")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Single server → no prefix
        let result = registry
            .route_tool_call("echo", json!({"msg": "hi"}))
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["msg"], "hi");
    }

    #[tokio::test]
    async fn test_route_tool_call_multi_server() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "fs-local".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("fs-local".into()),
            )
            .await;
        registry
            .register(
                "fs-remote".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("fs-remote".into()),
            )
            .await;

        let result = registry
            .route_tool_call("fs-local__read", json!({}))
            .await
            .unwrap();
        assert_eq!(result["called"], "read");

        let result = registry
            .route_tool_call("fs-remote__read", json!({}))
            .await
            .unwrap();
        assert_eq!(result["called"], "read");
    }

    #[tokio::test]
    async fn test_route_invalid_prefix() {
        let registry = AdapterRegistry::new();
        let result = registry.route_tool_call("bad_name", json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_route_missing_endpoint() {
        let registry = AdapterRegistry::new();
        let result = registry
            .route_tool_call("nonexistent__tool", json!({}))
            .await;
        assert!(result.is_err());
    }

    // --- Register/remove ---

    #[tokio::test]
    async fn test_register_and_remove() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        assert_eq!(registry.merged_catalog().await.len(), 1);

        let removed = registry.remove("ep").await;
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().transport, "stdio");
        assert!(registry.merged_catalog().await.is_empty());
    }

    #[tokio::test]
    async fn test_remove_nonexistent() {
        let registry = AdapterRegistry::new();
        assert!(registry.remove("ghost").await.is_none());
    }

    #[tokio::test]
    async fn test_list_healthy() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "good".into(),
                Box::new(MockAdapter::healthy(vec![])),
                "stdio".into(),
                None,
                Some("good".into()),
            )
            .await;
        registry
            .register(
                "bad".into(),
                Box::new(MockAdapter::unhealthy()),
                "stdio".into(),
                None,
                Some("bad".into()),
            )
            .await;

        let healthy = registry.list_healthy().await;
        assert_eq!(healthy, vec!["good"]);
    }

    #[tokio::test]
    async fn test_empty_registry_catalog() {
        let registry = AdapterRegistry::new();
        assert!(registry.merged_catalog().await.is_empty());
    }

    #[tokio::test]
    async fn test_empty_registry_list_healthy() {
        let registry = AdapterRegistry::new();
        assert!(registry.list_healthy().await.is_empty());
    }

    #[tokio::test]
    async fn test_duplicate_name_overwrites() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("old_tool")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("new_tool")])),
                "sse".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // After overwrite, single adapter → no prefix
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "new_tool");
    }

    #[tokio::test]
    async fn test_route_to_unhealthy_endpoint() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "sick".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool("tool")])),
                "stdio".into(),
                None,
                Some("sick".into()),
            )
            .await;

        // Single server → no prefix
        let result = registry.route_tool_call("tool", json!({})).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("not healthy"));
    }

    #[tokio::test]
    async fn test_entries_accessor() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let entries = registry.entries().read().await;
        assert_eq!(entries.len(), 1);
        assert!(entries.contains_key("ep"));
    }

    #[tokio::test]
    async fn test_merged_catalog_multiple_tools_per_endpoint() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("read"),
                    make_tool("write"),
                    make_tool("delete"),
                ])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Single adapter → no prefix
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 3);
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(names.contains(&"delete"));
    }

    #[tokio::test]
    async fn test_disabled_endpoint_excluded_from_catalog() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        {
            let mut entries = registry.entries().write().await;
            entries.get_mut("ep").unwrap().disabled = true;
        }

        let catalog = registry.merged_catalog().await;
        assert!(catalog.is_empty());
    }

    #[tokio::test]
    async fn test_disabled_tool_excluded_from_catalog() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("read"),
                    make_tool("write"),
                ])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        {
            let mut entries = registry.entries().write().await;
            entries
                .get_mut("ep")
                .unwrap()
                .disabled_tools
                .insert("read".into());
        }

        // Single adapter → no prefix
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "write");
    }

    #[tokio::test]
    async fn test_disabled_tool_blocks_route_call() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("t")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        {
            let mut entries = registry.entries().write().await;
            entries
                .get_mut("ep")
                .unwrap()
                .disabled_tools
                .insert("t".into());
        }

        // The tool is disabled so it won't appear in lookup → route fails
        let result = registry.route_tool_call("t", json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_description_enrichment_with_endpoint_name() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "filesystem".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("filesystem".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].description.as_deref(),
            Some("[filesystem] read tool")
        );
    }

    #[tokio::test]
    async fn test_description_enrichment_with_custom_description() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "fs-server".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                Some("File System".into()),
                Some("fs-server".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].description.as_deref(),
            Some("[File System] read tool")
        );
    }

    #[tokio::test]
    async fn test_description_enrichment_tool_without_description() {
        let registry = AdapterRegistry::new();
        let tool_no_desc = ToolInfo {
            name: "ping".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        };
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![tool_no_desc])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].description.as_deref(), Some("[ep]"));
    }

    #[tokio::test]
    async fn test_dead_tools_appear_in_catalog_with_unavailable_prefix() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "broken".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![
                    make_tool("read"),
                    make_tool("write"),
                ])),
                "stdio".into(),
                None,
                Some("broken".into()),
            )
            .await;

        // Single adapter → no prefix
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let read_tool = catalog.iter().find(|t| t.name == "read").unwrap();
        assert_eq!(
            read_tool.description.as_deref(),
            Some("[⚠️ UNAVAILABLE] [broken] read tool")
        );

        let write_tool = catalog.iter().find(|t| t.name == "write").unwrap();
        assert_eq!(
            write_tool.description.as_deref(),
            Some("[⚠️ UNAVAILABLE] [broken] write tool")
        );
    }

    #[tokio::test]
    async fn test_dead_tools_with_custom_description() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "broken".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool("read")])),
                "stdio".into(),
                Some("My Server".into()),
                Some("broken".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog[0].description.as_deref(),
            Some("[⚠️ UNAVAILABLE] [My Server] read tool")
        );
    }

    #[tokio::test]
    async fn test_calling_dead_tool_returns_error() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "broken".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("broken".into()),
            )
            .await;

        // Single server → no prefix
        let result = registry.route_tool_call("read", json!({})).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("unavailable"));
        assert!(err_msg.contains("broken"));
    }

    // --- Multiple endpoints with tool_prefix ---

    #[tokio::test]
    async fn test_multiple_endpoints_with_tool_prefix() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "fs-local".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("read"),
                    make_tool("write"),
                ])),
                "stdio".into(),
                Some("Local FS".into()),
                Some("fs-local".into()),
            )
            .await;
        registry
            .register(
                "fs-remote".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("read"),
                    make_tool("write"),
                ])),
                "stdio".into(),
                Some("Remote FS".into()),
                Some("fs-remote".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 4);

        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"fs-local__read"));
        assert!(names.contains(&"fs-local__write"));
        assert!(names.contains(&"fs-remote__read"));
        assert!(names.contains(&"fs-remote__write"));

        let local_read = catalog.iter().find(|t| t.name == "fs-local__read").unwrap();
        assert_eq!(
            local_read.description.as_deref(),
            Some("[Local FS] read tool")
        );

        let remote_read = catalog
            .iter()
            .find(|t| t.name == "fs-remote__read")
            .unwrap();
        assert_eq!(
            remote_read.description.as_deref(),
            Some("[Remote FS] read tool")
        );
    }

    #[tokio::test]
    async fn test_mixed_healthy_and_unhealthy_with_overlapping_tools() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "fs-ok".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("fs-ok".into()),
            )
            .await;
        registry
            .register(
                "fs-down".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("fs-down".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let ok_tool = catalog.iter().find(|t| t.name == "fs-ok__read").unwrap();
        assert_eq!(ok_tool.description.as_deref(), Some("[fs-ok] read tool"));

        let down_tool = catalog.iter().find(|t| t.name == "fs-down__read").unwrap();
        assert_eq!(
            down_tool.description.as_deref(),
            Some("[⚠️ UNAVAILABLE] [fs-down] read tool")
        );
    }

    // --- Alphabetical sorting ---

    #[tokio::test]
    async fn test_merged_catalog_sorted_alphabetically() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("zebra"),
                    make_tool("alpha"),
                    make_tool("mango"),
                ])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 3);
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    }

    #[tokio::test]
    async fn test_merged_catalog_multi_endpoint_sorted() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "zserver".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("beta"),
                    make_tool("alpha"),
                ])),
                "stdio".into(),
                None,
                Some("zserver".into()),
            )
            .await;
        registry
            .register(
                "aserver".into(),
                Box::new(MockAdapter::healthy(vec![
                    make_tool("delta"),
                    make_tool("gamma"),
                ])),
                "stdio".into(),
                None,
                Some("aserver".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 4);
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        // Sorted by prefixed name: aserver__delta, aserver__gamma, zserver__alpha, zserver__beta
        assert_eq!(
            names,
            vec![
                "aserver__delta",
                "aserver__gamma",
                "zserver__alpha",
                "zserver__beta"
            ]
        );
    }

    // --- Catalog cache invalidation after adapter replacement ---

    #[tokio::test]
    async fn test_catalog_stale_after_direct_replacement_without_invalidation() {
        let registry = AdapterRegistry::new();
        // Register a StartingAdapter (list_tools returns empty vec)
        registry
            .register(
                "ep".into(),
                Box::new(StartingAdapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Build catalog → should be empty (StartingAdapter returns no tools)
        let catalog = registry.merged_catalog().await;
        assert!(catalog.is_empty(), "StartingAdapter should have no tools");

        // Replace the adapter directly via entries().write() (bypasses cache invalidation)
        {
            let mut entries = registry.entries().write().await;
            let entry = entries.get_mut("ep").unwrap();
            entry.adapter = Box::new(MockAdapter::healthy(vec![make_tool("gmail_send")]));
        }

        // Catalog should still be empty because cache was not invalidated
        let catalog = registry.merged_catalog().await;
        assert!(
            catalog.is_empty(),
            "Cache should be stale after direct replacement"
        );

        // Now invalidate the cache
        registry.invalidate_catalog_cache().await;

        // Catalog should now reflect the new adapter's tools
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "gmail_send");
    }

    #[tokio::test]
    async fn test_register_auto_invalidates_cache() {
        let registry = AdapterRegistry::new();
        // Register adapter A with tool "alpha"
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("alpha")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "alpha");

        // Register adapter B under the SAME name with tool "beta"
        // register() should auto-invalidate the cache
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("beta")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Catalog should reflect adapter B's tools without manual invalidation
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "beta");
    }

    #[tokio::test]
    async fn test_multiple_rapid_replacements() {
        let registry = AdapterRegistry::new();
        // Register StartingAdapter, build catalog (empty, cached)
        registry
            .register(
                "ep".into(),
                Box::new(StartingAdapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        let catalog = registry.merged_catalog().await;
        assert!(catalog.is_empty());

        // Replace 3 times via entries().write() with different tool sets
        let replacements = vec![
            vec![make_tool("v1_tool")],
            vec![make_tool("v2_a"), make_tool("v2_b")],
            vec![
                make_tool("final_x"),
                make_tool("final_y"),
                make_tool("final_z"),
            ],
        ];

        for tools in &replacements {
            {
                let mut entries = registry.entries().write().await;
                let entry = entries.get_mut("ep").unwrap();
                entry.adapter = Box::new(MockAdapter::healthy(tools.clone()));
            }
            registry.invalidate_catalog_cache().await;
        }

        // Final merged_catalog() should reflect only the last replacement's tools
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 3);
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"final_x"));
        assert!(names.contains(&"final_y"));
        assert!(names.contains(&"final_z"));
    }

    #[tokio::test]
    async fn test_invalidate_cache_with_nonexistent_entry() {
        let registry = AdapterRegistry::new();

        // Write a new entry directly to entries() for a name that was never registered
        {
            let mut entries = registry.entries().write().await;
            entries.insert(
                "new_ep".to_string(),
                RegisteredAdapter {
                    adapter: Box::new(MockAdapter::healthy(vec![make_tool("surprise")])),
                    transport: "stdio".to_string(),
                    description: None,
                    tool_prefix: Some("new_ep".to_string()),
                    last_activity: None,
                    disabled: false,
                    disabled_tools: HashSet::new(),
                    tool_cache: RwLock::new(None),
                    tool_cache_populate_lock: Mutex::new(()),
                    tools_changed_task: None,
                },
            );
        }

        // Invalidate cache — should not panic
        registry.invalidate_catalog_cache().await;

        // Verify merged_catalog() includes the new entry's tools
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "surprise");
    }

    // --- Per-adapter tools cache (T1) ---

    /// Mock adapter that records the number of `list_tools` invocations.
    struct CountingAdapter {
        tools: Vec<ToolInfo>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        delay: Option<std::time::Duration>,
    }

    impl CountingAdapter {
        fn new(tools: Vec<ToolInfo>) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    tools,
                    calls: calls.clone(),
                    delay: None,
                },
                calls,
            )
        }

        fn with_delay(mut self, delay: std::time::Duration) -> Self {
            self.delay = Some(delay);
            self
        }
    }

    #[async_trait]
    impl McpAdapter for CountingAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({}))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_cached_list_tools_hit_does_not_call_adapter() {
        let registry = AdapterRegistry::new();
        let (adapter, calls) = CountingAdapter::new(vec![make_tool("a"), make_tool("b")]);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let entries = registry.entries().read().await;
        let entry = entries.get("ep").unwrap();
        let first = entry.cached_list_tools().await.unwrap();
        let second = entry.cached_list_tools().await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "second cached_list_tools call should hit the cache"
        );
    }

    #[tokio::test]
    async fn test_cached_list_tools_populates_once_under_concurrency() {
        let registry = AdapterRegistry::new();
        let (adapter, calls) = CountingAdapter::new(vec![make_tool("a")]);
        let adapter = adapter.with_delay(std::time::Duration::from_millis(50));
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let registry = Arc::new(registry);
        let mut handles = Vec::new();
        for _ in 0..10 {
            let reg = registry.clone();
            handles.push(tokio::spawn(async move {
                let entries = reg.entries().read().await;
                let entry = entries.get("ep").unwrap();
                entry.cached_list_tools().await.unwrap()
            }));
        }
        for h in handles {
            let tools = h.await.unwrap();
            assert_eq!(tools.len(), 1);
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "concurrent cached_list_tools calls must coalesce into a single populate"
        );
    }

    #[tokio::test]
    async fn test_invalidate_endpoint_tool_cache_clears_only_target() {
        let registry = AdapterRegistry::new();
        let (a1, calls1) = CountingAdapter::new(vec![make_tool("x")]);
        let (a2, calls2) = CountingAdapter::new(vec![make_tool("y")]);
        registry
            .register(
                "ep1".into(),
                Box::new(a1),
                "stdio".into(),
                None,
                Some("ep1".into()),
            )
            .await;
        registry
            .register(
                "ep2".into(),
                Box::new(a2),
                "stdio".into(),
                None,
                Some("ep2".into()),
            )
            .await;

        // Prime both caches.
        {
            let entries = registry.entries().read().await;
            entries
                .get("ep1")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
            entries
                .get("ep2")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
        }
        assert_eq!(calls1.load(Ordering::Relaxed), 1);
        assert_eq!(calls2.load(Ordering::Relaxed), 1);

        // Invalidate only ep1.
        registry.invalidate_endpoint_tool_cache("ep1").await;

        {
            let entries = registry.entries().read().await;
            entries
                .get("ep1")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
            entries
                .get("ep2")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
        }
        assert_eq!(
            calls1.load(Ordering::Relaxed),
            2,
            "ep1 cache should be invalidated and refetched"
        );
        assert_eq!(
            calls2.load(Ordering::Relaxed),
            1,
            "ep2 cache should remain intact"
        );
    }

    #[tokio::test]
    async fn test_invalidate_all_tool_caches_clears_every_entry() {
        let registry = AdapterRegistry::new();
        let (a1, calls1) = CountingAdapter::new(vec![make_tool("x")]);
        let (a2, calls2) = CountingAdapter::new(vec![make_tool("y")]);
        registry
            .register(
                "ep1".into(),
                Box::new(a1),
                "stdio".into(),
                None,
                Some("ep1".into()),
            )
            .await;
        registry
            .register(
                "ep2".into(),
                Box::new(a2),
                "stdio".into(),
                None,
                Some("ep2".into()),
            )
            .await;

        {
            let entries = registry.entries().read().await;
            entries
                .get("ep1")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
            entries
                .get("ep2")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
        }
        registry.invalidate_all_tool_caches().await;
        {
            let entries = registry.entries().read().await;
            entries
                .get("ep1")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
            entries
                .get("ep2")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
        }
        assert_eq!(calls1.load(Ordering::Relaxed), 2);
        assert_eq!(calls2.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_invalidate_catalog_cache_clears_per_endpoint_caches() {
        let registry = AdapterRegistry::new();
        let (adapter, calls) = CountingAdapter::new(vec![make_tool("a")]);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // First merged_catalog populates the per-endpoint cache.
        let _ = registry.merged_catalog().await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // A second merged_catalog hits the catalog cache (no list_tools call).
        let _ = registry.merged_catalog().await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Global invalidation should clear the per-endpoint cache too,
        // forcing a fresh list_tools on the next rebuild.
        registry.invalidate_catalog_cache().await;
        let _ = registry.merged_catalog().await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "invalidate_catalog_cache must clear per-endpoint caches"
        );
    }

    #[tokio::test]
    async fn test_invalidate_endpoint_tool_cache_bumps_generation_and_clears_catalog() {
        let registry = AdapterRegistry::new();
        let (adapter, _calls) = CountingAdapter::new(vec![make_tool("a")]);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let _ = registry.merged_catalog().await;
        let gen_before = registry.catalog_generation();
        registry.invalidate_endpoint_tool_cache("ep").await;
        assert!(registry.catalog_generation() > gen_before);
        // catalog_cache should be cleared; merged_catalog rebuilds.
        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
    }

    #[tokio::test]
    async fn test_invalidate_endpoint_tool_cache_unknown_name_is_noop() {
        let registry = AdapterRegistry::new();
        let (adapter, _calls) = CountingAdapter::new(vec![make_tool("a")]);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        // Should not panic and should still bump generation / clear catalog.
        let gen_before = registry.catalog_generation();
        registry
            .invalidate_endpoint_tool_cache("does-not-exist")
            .await;
        assert!(registry.catalog_generation() > gen_before);
    }

    // --- Tools-changed listener (T2) ---

    /// Mock adapter exposing a `broadcast::Sender<()>` so tests can drive
    /// `subscribe_tools_changed` ticks. Each `list_tools` invocation is
    /// counted via the shared atomic.
    struct NotifyingAdapter {
        tools: Vec<ToolInfo>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        tx: broadcast::Sender<()>,
    }

    impl NotifyingAdapter {
        fn new(
            tools: Vec<ToolInfo>,
            capacity: usize,
        ) -> (
            Self,
            Arc<std::sync::atomic::AtomicUsize>,
            broadcast::Sender<()>,
        ) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (tx, _) = broadcast::channel(capacity);
            (
                Self {
                    tools,
                    calls: calls.clone(),
                    tx: tx.clone(),
                },
                calls,
                tx,
            )
        }
    }

    #[async_trait]
    impl McpAdapter for NotifyingAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({}))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        fn subscribe_tools_changed(&self) -> Option<broadcast::Receiver<()>> {
            Some(self.tx.subscribe())
        }
    }

    /// Spin until `cond` returns true or the deadline elapses.
    async fn await_until<F>(timeout: std::time::Duration, mut cond: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn test_tools_changed_tick_invalidates_cache() {
        let registry = AdapterRegistry::new();
        let (adapter, calls, tx) = NotifyingAdapter::new(vec![make_tool("a")], 8);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Populate the per-endpoint cache (call #1).
        {
            let entries = registry.entries().read().await;
            entries
                .get("ep")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Send a tick and wait for the listener to invalidate.
        let gen_before = registry.catalog_generation();
        tx.send(()).unwrap();
        let observed = await_until(std::time::Duration::from_secs(1), || {
            registry.catalog_generation() > gen_before
        })
        .await;
        assert!(observed, "catalog generation should advance after tick");

        // Next cached_list_tools should re-invoke the adapter (call #2).
        {
            let entries = registry.entries().read().await;
            entries
                .get("ep")
                .unwrap()
                .cached_list_tools()
                .await
                .unwrap();
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "cache should have been cleared by the tools-changed tick"
        );
    }

    #[tokio::test]
    async fn test_remove_aborts_listener_task() {
        let registry = AdapterRegistry::new();
        let (adapter, _calls, tx) = NotifyingAdapter::new(vec![make_tool("a")], 8);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Sender should observe one receiver (the listener task).
        assert_eq!(tx.receiver_count(), 1);

        let removed = registry.remove("ep").await;
        assert!(removed.is_some());

        // After remove + abort, the listener task drops its receiver.
        let dropped = await_until(std::time::Duration::from_secs(1), || {
            tx.receiver_count() == 0
        })
        .await;
        assert!(
            dropped,
            "listener receiver should be dropped after remove()"
        );

        // Sending after remove must not panic and must not affect the registry.
        let gen_before = registry.catalog_generation();
        // No active receivers → send returns Err, which we deliberately ignore.
        let _ = tx.send(());
        // Give any (lingering) tasks a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            registry.catalog_generation(),
            gen_before,
            "no listener should remain to bump the generation"
        );
    }

    /// A per-endpoint adapter tick must also fan into the relay-wide
    /// `subscribe_tools_changed` channel so server-side consumers (e.g. the
    /// `/mcp/sse` handler) can forward `notifications/tools/list_changed`
    /// to MCP clients. The fanned-in payload carries the endpoint name so
    /// per-profile SSE handlers can filter by endpoint membership.
    #[tokio::test]
    async fn test_tools_changed_tick_fans_in_to_relay_subscribers() {
        let registry = AdapterRegistry::new();
        let mut relay_rx = registry.subscribe_tools_changed();
        let (adapter, _calls, tx) = NotifyingAdapter::new(vec![make_tool("a")], 8);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Drain the tick emitted by `register` itself so the next recv
        // observes only the fanned-in per-endpoint tick.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), relay_rx.recv()).await;

        // Drive a per-endpoint tick; the listener loop should observe it and
        // fan it into the relay-wide broadcast with the endpoint name.
        tx.send(()).unwrap();
        let recv = tokio::time::timeout(std::time::Duration::from_secs(1), relay_rx.recv()).await;
        match recv {
            Ok(Ok(name)) => assert_eq!(name, "ep", "fanned-in payload should be the endpoint name"),
            other => panic!(
                "relay-wide tools-changed subscriber should receive the fanned-in tick \
                 carrying the endpoint name (got {other:?})"
            ),
        }
    }

    /// `register` must emit exactly one relay-wide tools-changed tick
    /// carrying the endpoint name, after caches are invalidated.
    #[tokio::test]
    async fn test_register_emits_single_tools_changed_tick() {
        let registry = AdapterRegistry::new();
        let mut rx = registry.subscribe_tools_changed();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("a")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("register tick must fire within 1s")
            .expect("broadcast must still be open");
        assert_eq!(first, "ep");

        // Exactly one tick — a second recv must time out.
        let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            second.is_err(),
            "register must emit exactly one tick (got extra: {second:?})"
        );
    }

    /// `remove` on an existing endpoint must emit exactly one tick
    /// carrying the endpoint name.
    #[tokio::test]
    async fn test_remove_emits_single_tools_changed_tick() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("a")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Subscribe AFTER register so we only observe the remove tick.
        let mut rx = registry.subscribe_tools_changed();
        let removed = registry.remove("ep").await;
        assert!(removed.is_some(), "endpoint should have been removed");

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("remove tick must fire within 1s")
            .expect("broadcast must still be open");
        assert_eq!(first, "ep");

        let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            second.is_err(),
            "remove must emit exactly one tick (got extra: {second:?})"
        );
    }

    /// `remove` of a non-existent endpoint must NOT emit a tick.
    #[tokio::test]
    async fn test_remove_noop_emits_no_tools_changed_tick() {
        let registry = AdapterRegistry::new();
        let mut rx = registry.subscribe_tools_changed();
        let removed = registry.remove("does-not-exist").await;
        assert!(removed.is_none(), "no-op remove must return None");

        let any = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            any.is_err(),
            "no-op remove must not emit any tick (got: {any:?})"
        );
    }

    /// Re-registering an endpoint (overwriting an existing entry) must
    /// emit exactly one tick per register call — not two.
    #[tokio::test]
    async fn test_reregister_emits_single_tools_changed_tick() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("v1")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Subscribe AFTER the first register so we only observe the
        // overwrite tick.
        let mut rx = registry.subscribe_tools_changed();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("v2")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("overwrite register tick must fire within 1s")
            .expect("broadcast must still be open");
        assert_eq!(first, "ep");

        let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            second.is_err(),
            "overwrite register must emit exactly one tick (got extra: {second:?})"
        );
    }

    #[tokio::test]
    async fn test_watcher_swap_rewires_listener() {
        let registry = AdapterRegistry::new();
        let (a1, _calls1, tx_old) = NotifyingAdapter::new(vec![make_tool("old")], 8);
        registry
            .register(
                "ep".into(),
                Box::new(a1),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        assert_eq!(tx_old.receiver_count(), 1);

        // Simulate the watcher swap: replace adapter in place, then rewire.
        let (a2, _calls2, tx_new) = NotifyingAdapter::new(vec![make_tool("new")], 8);
        {
            let mut entries = registry.entries().write().await;
            entries.get_mut("ep").unwrap().adapter = Box::new(a2);
        }
        registry.rewire_tools_changed_listener("ep").await;

        // Old sender's listener must be torn down; new sender must have one.
        let old_dropped = await_until(std::time::Duration::from_secs(1), || {
            tx_old.receiver_count() == 0
        })
        .await;
        assert!(old_dropped, "old listener should be aborted on swap");
        assert_eq!(tx_new.receiver_count(), 1);

        // Tick on the OLD sender must NOT bump the registry.
        let gen_before = registry.catalog_generation();
        let _ = tx_old.send(());
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            registry.catalog_generation(),
            gen_before,
            "old sender ticks must be ignored after swap"
        );

        // Tick on the NEW sender must bump the registry.
        tx_new.send(()).unwrap();
        let observed = await_until(std::time::Duration::from_secs(1), || {
            registry.catalog_generation() > gen_before
        })
        .await;
        assert!(observed, "new sender ticks must drive invalidation");
    }

    #[tokio::test]
    async fn test_tools_changed_lagged_invalidates() {
        let registry = AdapterRegistry::new();
        // Capacity = 1: any burst beyond a single buffered message forces
        // `Lagged` on the next `recv()` after the listener drains.
        let (adapter, _calls, tx) = NotifyingAdapter::new(vec![make_tool("a")], 1);
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        // Drive one Ok tick first so the listener is back at `recv()` with
        // an empty buffer. Catalog generation bumps twice per tick (per-endpoint
        // cache + catalog cache).
        tx.send(()).unwrap();
        let _ = await_until(std::time::Duration::from_secs(1), || {
            registry.catalog_generation() >= 3
        })
        .await;
        let gen_after_first = registry.catalog_generation();

        // Now send a burst that overruns capacity. The listener will observe
        // exactly one `Lagged` on its next `recv()`, which must still trigger
        // an invalidation per the spec.
        for _ in 0..5 {
            let _ = tx.send(());
        }

        let observed = await_until(std::time::Duration::from_secs(1), || {
            registry.catalog_generation() > gen_after_first
        })
        .await;
        assert!(
            observed,
            "Lagged ticks must still trigger cache invalidation"
        );
    }

    /// rc.5 regression: when the adapter is not `Healthy` (for example an
    /// OAuth adapter pinned in `Refreshing` reports `Starting`),
    /// `cached_list_tools` must bypass the per-adapter cache and delegate
    /// straight to the adapter. Without the bypass, a previously cached
    /// "good" tool list — captured back when the adapter was Healthy —
    /// continues to be served indefinitely while the adapter is stuck,
    /// pinning the merged catalog and short-circuiting the reactive
    /// 401-driven refresh path inside the adapter's own `list_tools`.
    #[tokio::test]
    async fn cached_list_tools_bypasses_cache_when_not_healthy() {
        let registry = AdapterRegistry::new();

        // 1. Register a Healthy adapter and prime the cache with its tools.
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("cached_tool")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        {
            let entries = registry.entries().read().await;
            let entry = entries.get("ep").unwrap();
            let primed = entry.cached_list_tools().await.unwrap();
            assert_eq!(primed.len(), 1);
            assert_eq!(primed[0].name, "cached_tool");
        }

        // 2. Swap the adapter for a not-Healthy one (still has tools).
        // Crucially we do NOT call `invalidate_endpoint_tool_cache` —
        // that path is the existing escape hatch; this test exercises the
        // bypass behaviour while the stale cache is still resident.
        {
            let mut entries = registry.entries().write().await;
            let entry = entries.get_mut("ep").unwrap();
            entry.adapter = Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool(
                "live_tool",
            )]));
        }

        // 3. With the fix, `cached_list_tools` sees a non-Healthy adapter
        // and delegates to `adapter.list_tools()`, returning the live
        // `live_tool`. Without the fix, the stale cache is served and the
        // returned list contains `cached_tool`.
        let entries = registry.entries().read().await;
        let entry = entries.get("ep").unwrap();
        let live = entry.cached_list_tools().await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(
            live[0].name, "live_tool",
            "expected cached_list_tools to bypass the stale cache for a non-Healthy adapter and return live tools"
        );

        // 4. The bypass must NOT poison the cache by overwriting it with
        // tools fetched while the adapter was unhealthy. Confirm the cache
        // still contains the previously-primed entry. (If a future change
        // populates the cache from the bypass branch, the asserted name
        // would flip and force a deliberate review here.)
        let cached_after = entry.tool_cache.read().await.clone();
        let cached_after = cached_after.expect("cache must remain populated after bypass");
        assert_eq!(cached_after.len(), 1);
        assert_eq!(
            cached_after[0].name, "cached_tool",
            "bypass must not write through to the cache"
        );
    }

    /// Mock adapter that starts `Unhealthy` and flips to `Healthy` during
    /// its first `list_tools` invocation. Used by
    /// `cached_list_tools_coalesces_under_unhealthy_recovery` to model an
    /// OAuth adapter that recovers via a reactive refresh inside
    /// `list_tools` while a herd of concurrent callers is parked on the
    /// per-adapter populate lock.
    struct FlipAdapter {
        tools: Vec<ToolInfo>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        healthy: Arc<std::sync::atomic::AtomicBool>,
        delay: std::time::Duration,
    }

    impl FlipAdapter {
        fn new(
            tools: Vec<ToolInfo>,
            delay: std::time::Duration,
        ) -> (
            Self,
            Arc<std::sync::atomic::AtomicUsize>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let healthy = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let adapter = Self {
                tools,
                calls: calls.clone(),
                healthy: healthy.clone(),
                delay,
            };
            (adapter, calls, healthy)
        }
    }

    #[async_trait]
    impl McpAdapter for FlipAdapter {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            // Hold the populate lock long enough for a concurrent caller
            // to enqueue behind us, then flip to `Healthy` so the second
            // caller observes recovery on its re-check.
            tokio::time::sleep(self.delay).await;
            self.healthy.store(true, Ordering::Relaxed);
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({}))
        }
        fn health(&self) -> HealthStatus {
            if self.healthy.load(Ordering::Relaxed) {
                HealthStatus::Healthy
            } else {
                HealthStatus::Unhealthy("recovering".into())
            }
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    /// rc.5 regression: when two callers concurrently enter
    /// `cached_list_tools` on a not-`Healthy` adapter, the second caller
    /// must serialize behind the per-adapter populate lock and observe
    /// the cache populated by the first caller's recovered call — not
    /// trigger an independent `list_tools` invocation. Without coalescing
    /// (the lost herd protection from the original T-Fix-2), a herd of N
    /// callers would each drive their own `list_tools` and, for OAuth
    /// adapters, a per-caller refresh attempt past the in-flight dedup
    /// guard (observed in CI as `test_oauth_thundering_herd_single_refresh`
    /// reporting 25 refresh POSTs against a `<= 10` bound).
    #[tokio::test]
    async fn cached_list_tools_coalesces_under_unhealthy_recovery() {
        let registry = AdapterRegistry::new();
        let (adapter, calls, _healthy) = FlipAdapter::new(
            vec![make_tool("recovered_tool")],
            std::time::Duration::from_millis(50),
        );
        registry
            .register(
                "ep".into(),
                Box::new(adapter),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let registry = Arc::new(registry);
        let r1 = registry.clone();
        let r2 = registry.clone();

        // Spawn caller 1, then a tiny stagger so it wins the race to
        // acquire the populate lock before caller 2 enters.
        let h1 = tokio::spawn(async move {
            let entries = r1.entries().read().await;
            let entry = entries.get("ep").unwrap();
            entry.cached_list_tools().await
        });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let h2 = tokio::spawn(async move {
            let entries = r2.entries().read().await;
            let entry = entries.get("ep").unwrap();
            entry.cached_list_tools().await
        });

        let result1 = h1.await.unwrap().expect("caller 1 list_tools");
        let result2 = h2.await.unwrap().expect("caller 2 list_tools");

        assert_eq!(result1.len(), 1);
        assert_eq!(result1[0].name, "recovered_tool");
        assert_eq!(result2.len(), 1);
        assert_eq!(result2[0].name, "recovered_tool");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "expected exactly one underlying list_tools call; the second \
             caller must observe the populated cache after the populate lock"
        );
    }

    // --- Early-rejection event emission (route_tool_call) ---
    //
    // Each of the 5 early-rejection branches in `route_tool_call` must
    // publish a `Started` + `Failed` pair on the wired
    // `ToolCallEventBus` before returning the underlying
    // `AdapterError::ProtocolError`. The error string must match the
    // pre-event wording exactly (log scrapers depend on it).

    /// Drain the next two events from `rx` and assert they form a matched
    /// `Started` + `Failed` pair carrying the expected tool name and error
    /// message. Returns the request_id so the caller can assert nothing
    /// else followed.
    async fn assert_started_then_failed(
        rx: &mut tokio::sync::broadcast::Receiver<ToolCallEvent>,
        expected_tool: &str,
        expected_endpoint: Option<&str>,
        expected_error: &str,
    ) -> String {
        let started = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("started event arrived")
            .expect("started recv ok");
        let (sid, sendpoint) = match started {
            ToolCallEvent::Started {
                request_id,
                tool,
                endpoint,
                ..
            } => {
                assert_eq!(tool, expected_tool, "Started.tool");
                if let Some(exp) = expected_endpoint {
                    assert_eq!(endpoint, exp, "Started.endpoint");
                }
                (request_id, endpoint)
            }
            other => panic!("expected Started, got {:?}", other),
        };
        let _ = sendpoint;
        let failed = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("failed event arrived")
            .expect("failed recv ok");
        match failed {
            ToolCallEvent::Failed {
                request_id,
                status,
                error_message,
                ..
            } => {
                assert_eq!(request_id, sid, "Failed.request_id matches Started");
                assert_eq!(status, "error");
                assert_eq!(error_message, expected_error);
            }
            other => panic!("expected Failed, got {:?}", other),
        }
        sid
    }

    #[tokio::test]
    async fn route_unknown_prefix_emits_started_and_failed() {
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);

        let err = registry
            .route_tool_call("ghost_tool", json!({}))
            .await
            .expect_err("unknown prefix should reject");
        let expected = "no tool found for prefixed name 'ghost_tool'";
        match &err {
            AdapterError::ProtocolError(msg) => assert_eq!(msg, expected),
            other => panic!("expected ProtocolError, got {:?}", other),
        }
        assert_started_then_failed(&mut rx, "ghost_tool", Some("unknown"), expected).await;
    }

    #[tokio::test]
    async fn route_missing_adapter_emits_started_and_failed() {
        // Build a lookup pointing at endpoint `ep` then remove the
        // adapter without invalidating the cached lookup so route hits
        // the "no adapter found" branch.
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        // Warm the cached lookup so the subsequent removal keeps the
        // (prefixed_name -> ("ep", "read")) mapping in `catalog_cache`.
        let _ = registry.merged_catalog_with_lookup().await;
        // Bypass `remove()` (which invalidates the catalog) by yanking
        // the adapter directly out of the inner map.
        registry.adapters.write().await.remove("ep");

        let err = registry
            .route_tool_call("read", json!({}))
            .await
            .expect_err("missing adapter should reject");
        let expected = "no adapter found for endpoint 'ep'";
        match &err {
            AdapterError::ProtocolError(msg) => assert_eq!(msg, expected),
            other => panic!("expected ProtocolError, got {:?}", other),
        }
        assert_started_then_failed(&mut rx, "read", Some("ep"), expected).await;
    }

    #[tokio::test]
    async fn route_disabled_endpoint_emits_started_and_failed() {
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        // Warm the lookup, then disable the endpoint without
        // invalidating so the branch is reachable.
        let _ = registry.merged_catalog_with_lookup().await;
        registry
            .adapters
            .write()
            .await
            .get_mut("ep")
            .unwrap()
            .disabled = true;

        let err = registry
            .route_tool_call("read", json!({}))
            .await
            .expect_err("disabled endpoint should reject");
        let expected = "endpoint 'ep' is disabled";
        match &err {
            AdapterError::ProtocolError(msg) => assert_eq!(msg, expected),
            other => panic!("expected ProtocolError, got {:?}", other),
        }
        assert_started_then_failed(&mut rx, "read", Some("ep"), expected).await;
    }

    #[tokio::test]
    async fn route_unhealthy_endpoint_emits_started_and_failed() {
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);
        // Register an unhealthy adapter that still publishes a tool so
        // the lookup contains it (the catalog includes unhealthy tools
        // marked `[⚠️ UNAVAILABLE]`).
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let err = registry
            .route_tool_call("read", json!({}))
            .await
            .expect_err("unhealthy endpoint should reject");
        let expected = "tool 'read' is currently unavailable: endpoint 'ep' is not healthy";
        match &err {
            AdapterError::ProtocolError(msg) => assert_eq!(msg, expected),
            other => panic!("expected ProtocolError, got {:?}", other),
        }
        assert_started_then_failed(&mut rx, "read", Some("ep"), expected).await;
    }

    #[tokio::test]
    async fn route_disabled_tool_emits_started_and_failed() {
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        // Warm the lookup so the disabled-tool branch becomes reachable
        // (merged_catalog_with_lookup filters disabled tools, so a
        // post-disable lookup rebuild would route the call through the
        // unknown-prefix branch instead).
        let _ = registry.merged_catalog_with_lookup().await;
        registry
            .adapters
            .write()
            .await
            .get_mut("ep")
            .unwrap()
            .disabled_tools
            .insert("read".into());

        let err = registry
            .route_tool_call("read", json!({}))
            .await
            .expect_err("disabled tool should reject");
        let expected = "tool 'read' is disabled on endpoint 'ep'";
        match &err {
            AdapterError::ProtocolError(msg) => assert_eq!(msg, expected),
            other => panic!("expected ProtocolError, got {:?}", other),
        }
        assert_started_then_failed(&mut rx, "read", Some("ep"), expected).await;
    }

    #[tokio::test]
    async fn route_success_does_not_emit_registry_events() {
        // Sanity check: when the call reaches the adapter (and the mock
        // adapter does not publish events), the registry must not emit
        // a stray `Started`/`Failed` pair.
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let result = registry.route_tool_call("read", json!({})).await.unwrap();
        assert_eq!(result["called"], "read");
        let try_recv = rx.try_recv();
        assert!(
            matches!(
                try_recv,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "registry must not publish events on the happy path; got {:?}",
            try_recv
        );
    }

    // --- Input schema validation (route_tool_call) ---
    //
    // These tests exercise the validation layer wired into
    // `route_tool_call`. Slice 2 replaces the basic
    // `ValidationError::to_string()` rendering with the per-kind message
    // table (§5.2), dot/bracket path rendering (§5.3), and close-match /
    // coercion suggestions (§5.4). Assertions pin the `isError: true`
    // shape, the `format_validation_message` wrapper text, and the
    // human-readable per-field fragments and suggestions.

    fn make_tool_with_schema(name: &str, schema: serde_json::Value) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{} tool", name)),
            input_schema: schema,
            annotations: None,
        }
    }

    /// Register a single healthy adapter exposing one tool with `schema`.
    /// Single-server no-prefix mode means the routed name equals `tool`.
    async fn registry_with_schema(tool: &str, schema: serde_json::Value) -> AdapterRegistry {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool_with_schema(
                    tool, schema,
                )])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;
        registry
    }

    fn assert_validation_error(result: &serde_json::Value, tool: &str) -> String {
        assert_eq!(result["isError"], json!(true), "expected isError: true");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("content text present")
            .to_string();
        assert!(
            text.contains(&format!("Input validation failed for tool '{}'", tool)),
            "message should carry the wrapper header: {}",
            text
        );
        text
    }

    #[tokio::test]
    async fn validate_required_field_missing() {
        let registry = registry_with_schema(
            "create",
            json!({
                "type": "object",
                "properties": { "repo": { "type": "string" }, "title": { "type": "string" } },
                "required": ["repo", "title"],
            }),
        )
        .await;
        let result = registry
            .route_tool_call("create", json!({ "repo": "x" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "create");
        assert!(
            text.contains("title"),
            "should name the missing field: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_type_mismatch() {
        let registry = registry_with_schema(
            "count",
            json!({
                "type": "object",
                "properties": { "count": { "type": "number" } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("count", json!({ "count": "five" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "count");
        assert!(
            text.contains("number"),
            "should mention expected type: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_enum_violation() {
        let registry = registry_with_schema(
            "set",
            json!({
                "type": "object",
                "properties": { "priority": { "enum": ["low", "medium", "high"] } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("set", json!({ "priority": "critical" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "set");
        assert!(
            text.contains("critical"),
            "should echo the bad value: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_additional_properties_rejected() {
        let registry = registry_with_schema(
            "note",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "additionalProperties": false,
            }),
        )
        .await;
        let result = registry
            .route_tool_call("note", json!({ "text": "hi", "zzz": 1 }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "note");
        assert!(
            text.contains("zzz"),
            "should name the unknown parameter: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_additional_properties_true_passes() {
        let registry = registry_with_schema(
            "note",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "additionalProperties": true,
            }),
        )
        .await;
        let result = registry
            .route_tool_call("note", json!({ "text": "hi", "extra": 1 }))
            .await
            .unwrap();
        assert_eq!(result["called"], "note", "valid input should dispatch");
    }

    #[tokio::test]
    async fn validate_min_length_violation() {
        let registry = registry_with_schema(
            "name",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string", "minLength": 3 } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("name", json!({ "name": "ab" }))
            .await
            .unwrap();
        // §10.1 #8: the message reports the offending string length.
        let text = assert_validation_error(&result, "name");
        assert!(
            text.contains("string length 2"),
            "should report the string length: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_maximum_violation() {
        let registry = registry_with_schema(
            "count",
            json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "maximum": 100 } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("count", json!({ "count": 150 }))
            .await
            .unwrap();
        // §10.1 #9: the message frames the violation as a range problem.
        let text = assert_validation_error(&result, "count");
        assert!(
            text.contains("150") && text.contains("outside range"),
            "should echo the value and the range phrasing: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_type_mismatch_array_coercion_suggestion() {
        // §10.1 #3: a comma-containing string for an array field suggests the
        // array form.
        let registry = registry_with_schema(
            "label",
            json!({
                "type": "object",
                "properties": { "labels": { "type": "array" } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("label", json!({ "labels": "bug,urgent" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "label");
        assert!(
            text.contains("expected array"),
            "should name the expected type: {}",
            text
        );
        assert!(
            text.contains("Did you mean [\"bug\", \"urgent\"]"),
            "should suggest the array form: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_enum_close_match_suggestion() {
        // §10.1 #5: a near-miss enum value suggests the closest variant.
        let registry = registry_with_schema(
            "state",
            json!({
                "type": "object",
                "properties": { "state": { "enum": ["open", "closed"] } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("state", json!({ "state": "opne" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "state");
        assert!(
            text.contains("Did you mean \"open\""),
            "should suggest the closest enum variant: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_additional_properties_valid_params_list() {
        // §10.1 #7: an unknown parameter lists the valid parameter names.
        let registry = registry_with_schema(
            "note",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "additionalProperties": false,
            }),
        )
        .await;
        let result = registry
            .route_tool_call("note", json!({ "text": "hi", "zzz": 1 }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "note");
        assert!(
            text.contains("unknown parameter") && text.contains("Valid parameters: text"),
            "should list the valid parameters: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_pattern_violation() {
        // §10.1 #10: a pattern mismatch is reported as such.
        let registry = registry_with_schema(
            "slugify",
            json!({
                "type": "object",
                "properties": { "slug": { "type": "string", "pattern": "^[a-z-]+$" } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("slugify", json!({ "slug": "My Slug" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "slugify");
        assert!(
            text.contains("does not match pattern"),
            "should report a pattern mismatch: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_format_email_violation() {
        // §10.1 #11: a bad `format: email` value is reported by name.
        let registry = registry_with_schema(
            "invite",
            json!({
                "type": "object",
                "properties": { "email": { "type": "string", "format": "email" } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("invite", json!({ "email": "notanemail" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "invite");
        assert!(
            text.contains("not a valid email"),
            "should name the failing format: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_multiple_errors_collected() {
        // §10.1 #12: every error from a single pass is collected together.
        let registry = registry_with_schema(
            "multi",
            json!({
                "type": "object",
                "properties": { "a": { "type": "number" } },
                "required": ["a", "b"],
            }),
        )
        .await;
        let result = registry
            .route_tool_call("multi", json!({ "a": "x" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "multi");
        assert!(
            text.contains("'b': required field is missing"),
            "should report the missing required field: {}",
            text
        );
        assert!(
            text.contains("expected number"),
            "should report the type mismatch in the same pass: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_unknown_param_close_match() {
        // §10.1 #15: a misspelled parameter suggests the closest known name.
        let registry = registry_with_schema(
            "repo",
            json!({
                "type": "object",
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" }
                },
                "additionalProperties": false,
            }),
        )
        .await;
        let result = registry
            .route_tool_call("repo", json!({ "ownre": "x" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "repo");
        assert!(
            text.contains("Did you mean 'owner'"),
            "should suggest the closest known parameter: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_reports_all_unknown_params() {
        // The crate batches every unexpected key into one error; the formatter
        // must name them all (with a per-key suggestion) so the model can fix
        // every unknown parameter in a single retry.
        let registry = registry_with_schema(
            "repo",
            json!({
                "type": "object",
                "properties": {
                    "owner": { "type": "string" },
                    "repo": { "type": "string" }
                },
                "additionalProperties": false,
            }),
        )
        .await;
        let result = registry
            .route_tool_call("repo", json!({ "ownre": "x", "rep": "y" }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "repo");
        assert!(
            text.contains("'ownre'") && text.contains("'rep'"),
            "should name every unknown parameter: {}",
            text
        );
        assert!(
            text.contains("Did you mean 'owner' for 'ownre'"),
            "should suggest the closest match per unknown key: {}",
            text
        );
        assert!(
            text.contains("Did you mean 'repo' for 'rep'"),
            "should suggest the closest match per unknown key: {}",
            text
        );
        assert!(
            text.contains("Valid parameters: owner, repo"),
            "should still list the valid parameters: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_degenerate_schema_passes() {
        // `{"type": "object"}` with no constraints is not validatable, so the
        // call passes through to the adapter unchanged.
        let registry = registry_with_schema("any", json!({ "type": "object" })).await;
        let result = registry
            .route_tool_call("any", json!({ "anything": true }))
            .await
            .unwrap();
        assert_eq!(result["called"], "any", "degenerate schema should dispatch");
    }

    #[tokio::test]
    async fn validate_closed_schema_without_properties_rejects_extra() {
        // A closed schema that declares no `properties`/`required` still
        // forbids unexpected arguments and must be validated, not skipped.
        // Such a schema accepts nothing, so the actionable message is to
        // remove all arguments (the offending key name is not recoverable
        // from the crate's `FalseSchema` error).
        let registry = registry_with_schema(
            "noargs",
            json!({ "type": "object", "additionalProperties": false }),
        )
        .await;
        let result = registry
            .route_tool_call("noargs", json!({ "surprise": 1 }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "noargs");
        assert!(
            text.contains("accepts no parameters") && text.contains("remove all arguments"),
            "closed schema should reject the unexpected argument: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_closed_schema_without_properties_passes_when_empty() {
        // The same closed schema accepts an empty argument object.
        let registry = registry_with_schema(
            "noargs",
            json!({ "type": "object", "additionalProperties": false }),
        )
        .await;
        let result = registry.route_tool_call("noargs", json!({})).await.unwrap();
        assert_eq!(result["called"], "noargs", "no-arg call should dispatch");
    }

    #[tokio::test]
    async fn validate_ref_only_schema_is_enforced() {
        // A schema that constrains solely via `$ref` (no top-level
        // `properties`/`required`) must still be compiled and enforced rather
        // than treated as degenerate and passed through.
        let registry = registry_with_schema(
            "reffed",
            json!({
                "type": "object",
                "$ref": "#/$defs/payload",
                "$defs": {
                    "payload": {
                        "type": "object",
                        "properties": { "name": { "type": "string" } },
                        "required": ["name"],
                        "additionalProperties": false,
                    }
                },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("reffed", json!({ "nope": 1 }))
            .await
            .unwrap();
        assert_validation_error(&result, "reffed");
    }

    #[tokio::test]
    async fn validate_combinator_only_schema_is_enforced() {
        // A schema whose only constraint is a `oneOf` combinator must be
        // validated, not skipped.
        let registry = registry_with_schema(
            "combo",
            json!({
                "type": "object",
                "oneOf": [
                    { "required": ["a"] },
                    { "required": ["b"] }
                ],
            }),
        )
        .await;
        let result = registry.route_tool_call("combo", json!({})).await.unwrap();
        assert_validation_error(&result, "combo");
    }

    #[tokio::test]
    async fn validate_nested_additional_properties_lists_nested_keys() {
        // An unexpected key inside a nested object must report the nested
        // object's valid parameters, not the root's.
        let registry = registry_with_schema(
            "configure",
            json!({
                "type": "object",
                "properties": {
                    "config": {
                        "type": "object",
                        "properties": {
                            "host": { "type": "string" },
                            "port": { "type": "integer" }
                        },
                        "additionalProperties": false,
                    }
                },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("configure", json!({ "config": { "hostt": "x" } }))
            .await
            .unwrap();
        let text = assert_validation_error(&result, "configure");
        assert!(
            text.contains("config.hostt") && text.contains("unknown parameter"),
            "should name the nested unknown parameter with its dotted path: {}",
            text
        );
        assert!(
            text.contains("Valid parameters: host, port"),
            "should list the nested object's valid parameters: {}",
            text
        );
    }

    #[tokio::test]
    async fn validate_malformed_schema_passes() {
        // A schema that fails to compile (invalid `type` keyword value) must
        // not block the call — the relay defers to the upstream server.
        let registry = registry_with_schema(
            "weird",
            json!({
                "type": "object",
                "properties": { "x": { "type": 12345 } },
            }),
        )
        .await;
        let result = registry
            .route_tool_call("weird", json!({ "x": 1 }))
            .await
            .unwrap();
        assert_eq!(
            result["called"], "weird",
            "malformed schema should dispatch"
        );
        // The compilation failure is cached as `None` so it is not retried.
        let cache = registry.schema_cache.read().await;
        assert!(matches!(cache.get("weird"), Some(None)));
    }

    #[tokio::test]
    async fn validate_schema_compiled_once_and_cached() {
        let registry = registry_with_schema(
            "create",
            json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"],
            }),
        )
        .await;
        // First call populates the cache with a compiled validator.
        let _ = registry
            .route_tool_call("create", json!({ "repo": "x" }))
            .await;
        {
            let cache = registry.schema_cache.read().await;
            assert!(
                matches!(cache.get("create"), Some(Some(_))),
                "validator should be cached after first call"
            );
        }
        // Second call reuses it (no panic, still cached).
        let _ = registry
            .route_tool_call("create", json!({ "repo": "y" }))
            .await;
        let cache = registry.schema_cache.read().await;
        assert_eq!(cache.len(), 1, "no duplicate compilation entries");
    }

    #[tokio::test]
    async fn validate_cache_cleared_on_catalog_invalidation() {
        let registry = registry_with_schema(
            "create",
            json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"],
            }),
        )
        .await;
        let _ = registry
            .route_tool_call("create", json!({ "repo": "x" }))
            .await;
        assert!(!registry.schema_cache.read().await.is_empty());
        registry.invalidate_catalog_cache().await;
        assert!(
            registry.schema_cache.read().await.is_empty(),
            "schema cache must clear when the catalog is invalidated"
        );
    }

    #[tokio::test]
    async fn validate_error_before_upstream_dispatch() {
        // A validation failure returns the `isError: true` result without the
        // `called` marker the MockAdapter would add, proving the adapter was
        // never invoked.
        let registry = registry_with_schema(
            "create",
            json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"],
            }),
        )
        .await;
        let result = registry.route_tool_call("create", json!({})).await.unwrap();
        assert_validation_error(&result, "create");
        assert!(
            result.get("called").is_none(),
            "adapter must not be dispatched on validation failure"
        );
    }

    #[tokio::test]
    async fn validate_disabled_toggle_skips_validation() {
        let registry = registry_with_schema(
            "create",
            json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"],
            }),
        )
        .await
        .with_validate_inputs(false);
        // Missing required field, but validation is off, so it dispatches.
        let result = registry.route_tool_call("create", json!({})).await.unwrap();
        assert_eq!(
            result["called"], "create",
            "toggle off should bypass validation"
        );
    }

    #[tokio::test]
    async fn validate_failure_emits_validation_error_event() {
        let bus = ToolCallEventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        let registry = AdapterRegistry::new().with_event_bus(bus);
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool_with_schema(
                    "create",
                    json!({
                        "type": "object",
                        "properties": { "repo": { "type": "string" } },
                        "required": ["repo"],
                    }),
                )])),
                "stdio".into(),
                None,
                Some("ep".into()),
            )
            .await;

        let result = registry.route_tool_call("create", json!({})).await.unwrap();
        assert_validation_error(&result, "create");

        let started = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("started event arrived")
            .expect("started recv ok");
        let sid = match started {
            ToolCallEvent::Started {
                request_id, tool, ..
            } => {
                assert_eq!(tool, "create");
                request_id
            }
            other => panic!("expected Started, got {:?}", other),
        };
        let failed = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("failed event arrived")
            .expect("failed recv ok");
        match failed {
            ToolCallEvent::Failed {
                request_id, status, ..
            } => {
                assert_eq!(request_id, sid);
                assert_eq!(status, "validation_error");
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }
}
