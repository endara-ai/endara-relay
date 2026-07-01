//! Per-profile runtime registry — Slice 1, R2.A.
//!
//! A *profile* is a named subset of configured endpoints (see
//! [`crate::config::ProfileConfig`]). At runtime each profile is represented
//! by a [`ProfileContext`] held inside the relay-wide [`ProfileRegistry`].
//! Profile-scoped request handlers look the context up by URL path and use
//! its [`ProfileRegistryView`] in place of the full [`AdapterRegistry`] so
//! catalogue listings and tool calls only see the endpoints that belong to
//! that profile.
//!
//! The per-profile [`MetaToolHandler`] (Engineering Spec §3.5) is deferred to
//! task R3.A — that task will parameterise `MetaToolHandler` over a small
//! `MetaToolRegistry` trait. Until then [`ProfileContext::meta_tool_handler`]
//! is left `None`; profile-scoped HTTP handlers landing in R2.B/R3.A will
//! fill it in.
//!
//! The accessors below are flagged `#[allow(dead_code)]` because their only
//! callers (the profile-scoped HTTP/SSE handlers) land in R2.B / R3.A. The
//! attribute will be removed in the task that wires the first consumer.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{broadcast, RwLock};

use crate::adapter::{AdapterError, ToolInfo};
use crate::config::ProfileConfig;
use crate::js_sandbox::MetaToolHandler;
use crate::registry::{AdapterRegistry, MetaToolRegistry};

/// Runtime state for a single resolved profile.
///
/// Constructed by [`ProfileRegistry::rebuild`] and handed (as `Arc`) to the
/// profile-scoped request handlers landing in R2.B. Per-profile JS-execution
/// and TOON-output flags are copied verbatim from the profile config — the
/// loader requires concrete booleans per spec §2.4, so handlers don't fall
/// back to relay-wide defaults.
pub struct ProfileContext {
    /// The original profile configuration this context was built from.
    pub config: ProfileConfig,
    /// Filtered view over the global [`AdapterRegistry`].
    pub registry_view: ProfileRegistryView,
    /// Per-profile JS-execution flag (mirror of
    /// [`ProfileConfig::js_execution`]).
    pub js_execution: bool,
    /// Per-profile TOON-output flag (mirror of
    /// [`ProfileConfig::toon_output`]).
    pub toon_output: bool,
    /// Per-profile [`MetaToolHandler`]. Populated by R3.A once
    /// `MetaToolHandler` is parameterised over a `MetaToolRegistry` trait
    /// (recon §D3, locked decision #4). `None` at the moment.
    pub meta_tool_handler: Option<Arc<MetaToolHandler>>,
}

/// Filtered view over an [`AdapterRegistry`] that restricts catalogue
/// listings and tool routing to a fixed set of endpoint names.
///
/// Cheap to clone — internally `Arc`s the allowed-endpoints set and reuses
/// the registry's cached merged catalogue.
#[derive(Clone)]
pub struct ProfileRegistryView {
    inner: AdapterRegistry,
    allowed_endpoints: Arc<HashSet<String>>,
}

impl ProfileRegistryView {
    /// Build a view restricted to `allowed_endpoints`.
    pub fn new(inner: AdapterRegistry, allowed_endpoints: HashSet<String>) -> Self {
        Self {
            inner,
            allowed_endpoints: Arc::new(allowed_endpoints),
        }
    }

    /// Borrow the underlying [`AdapterRegistry`].
    pub fn inner(&self) -> &AdapterRegistry {
        &self.inner
    }

    /// The set of endpoint names this view exposes.
    pub fn allowed_endpoints(&self) -> &HashSet<String> {
        &self.allowed_endpoints
    }

    /// Merged catalogue filtered to tools whose owning endpoint is in
    /// [`Self::allowed_endpoints`].
    pub async fn merged_catalog(&self) -> Vec<ToolInfo> {
        let (catalog, lookup) = self.inner.merged_catalog_with_lookup().await;
        catalog
            .into_iter()
            .filter(|tool| {
                lookup
                    .get(&tool.name)
                    .map(|(endpoint, _)| self.allowed_endpoints.contains(endpoint))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Filtered variant of [`AdapterRegistry::merged_catalog_with_lookup`].
    pub async fn merged_catalog_with_lookup(
        &self,
    ) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        let (catalog, lookup) = self.inner.merged_catalog_with_lookup().await;
        let filtered_lookup: HashMap<String, (String, String)> = lookup
            .into_iter()
            .filter(|(_, (endpoint, _))| self.allowed_endpoints.contains(endpoint))
            .collect();
        let filtered_catalog = catalog
            .into_iter()
            .filter(|tool| filtered_lookup.contains_key(&tool.name))
            .collect();
        (filtered_catalog, filtered_lookup)
    }

    /// Route a prefixed tool call, rejecting any tool whose owning endpoint
    /// is not in [`Self::allowed_endpoints`].
    pub async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: Value,
    ) -> Result<Value, AdapterError> {
        let (_, lookup) = self.inner.merged_catalog_with_lookup().await;
        let (endpoint, _) = lookup.get(prefixed_name).ok_or_else(|| {
            AdapterError::ProtocolError(format!("no tool '{}' in profile", prefixed_name))
        })?;
        if !self.allowed_endpoints.contains(endpoint) {
            return Err(AdapterError::ProtocolError(format!(
                "tool '{}' is not available in this profile",
                prefixed_name
            )));
        }
        self.inner.route_tool_call(prefixed_name, arguments).await
    }

    /// Profile-scoped variant of
    /// [`AdapterRegistry::route_tool_call_with_request_params`]. Applies the
    /// same allowed-endpoint guard as [`Self::route_tool_call`], then forwards
    /// the extra top-level `tools/call` params (MCP 2026-07-28 multi round-trip
    /// `inputResponses`/`requestState`) verbatim to the underlying registry.
    pub async fn route_tool_call_with_request_params(
        &self,
        prefixed_name: &str,
        arguments: Value,
        request_params: serde_json::Map<String, Value>,
    ) -> Result<Value, AdapterError> {
        let (_, lookup) = self.inner.merged_catalog_with_lookup().await;
        let (endpoint, _) = lookup.get(prefixed_name).ok_or_else(|| {
            AdapterError::ProtocolError(format!("no tool '{}' in profile", prefixed_name))
        })?;
        if !self.allowed_endpoints.contains(endpoint) {
            return Err(AdapterError::ProtocolError(format!(
                "tool '{}' is not available in this profile",
                prefixed_name
            )));
        }
        self.inner
            .route_tool_call_with_request_params(prefixed_name, arguments, request_params)
            .await
    }

    /// Profile-scoped variant of [`AdapterRegistry::list_resources`].
    ///
    /// Aggregates `resources/list` across only the profile's allowed endpoints
    /// and wraps URIs using the *in-profile* active count so the wrap trigger
    /// matches [`Self::route_resource_read`]'s unwrap trigger — keeping the
    /// allowed-endpoint guard tight in the `(global>=2, profile==1)` corner.
    pub async fn list_resources(&self) -> Vec<Value> {
        self.inner.list_resources_in(&self.allowed_endpoints).await
    }

    /// Profile-scoped variant of [`AdapterRegistry::list_resource_templates`].
    /// See [`Self::list_resources`] for the wrap-trigger rationale.
    pub async fn list_resource_templates(&self) -> Vec<Value> {
        self.inner
            .list_resource_templates_in(&self.allowed_endpoints)
            .await
    }

    /// Profile-scoped variant of [`AdapterRegistry::list_prompts`].
    ///
    /// Aggregates `prompts/list` across only the profile's allowed endpoints
    /// and namespaces `prompts[].name` using the *in-profile* active count so
    /// the prefix trigger stays consistent with the profile-scoped tool
    /// catalog.
    pub async fn list_prompts(&self) -> Vec<Value> {
        self.inner.list_prompts_in(&self.allowed_endpoints).await
    }

    /// Profile-scoped variant of [`AdapterRegistry::route_prompt_get`].
    ///
    /// Rebuilds the reverse lookup from the *in-profile* `list_prompts_in_with_lookup`
    /// so the prefix-trigger matches what the profile-scoped `prompts/list`
    /// emitted; that closes the `(global>=2, profile==1)` foreign-prompt
    /// bypass the same way [`Self::route_resource_read`] does for resources.
    /// Rejects any prompt whose owning endpoint is outside the profile's
    /// allowed set before dispatch. Slot #9 URI rewriting is applied with
    /// the in-profile `skip_wrap` trigger so the wrap toggles in lockstep
    /// with the profile-scoped prompt-name prefix toggle.
    pub async fn route_prompt_get(
        &self,
        prefixed_name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let (_prompts, lookup) = self
            .inner
            .list_prompts_in_with_lookup(&self.allowed_endpoints)
            .await;
        let (endpoint, raw_name) = match lookup.get(prefixed_name) {
            Some(v) => v.clone(),
            None => {
                return Err(AdapterError::ProtocolError(format!(
                    "no prompt '{}' in profile",
                    prefixed_name
                )));
            }
        };
        if !self.allowed_endpoints.contains(&endpoint) {
            return Err(AdapterError::ProtocolError(format!(
                "prompt '{}' is not available in this profile",
                prefixed_name
            )));
        }
        let skip_wrap = self
            .inner
            .active_endpoint_count_in(&self.allowed_endpoints)
            .await
            <= 1;
        let value = self
            .inner
            .dispatch_prompt_get_to(&endpoint, &raw_name, arguments)
            .await?;
        Ok(crate::tool_call_rewrite::rewrite_prompt_get_result(
            value, &endpoint, skip_wrap,
        ))
    }

    /// Profile-scoped variant of [`AdapterRegistry::route_resource_read`].
    ///
    /// Always attempts a strict decode of `wrapped_uri` first — independent
    /// of the in-profile active count — so a fully-wrapped `mcp-relay://`
    /// URI is recognised even when the profile would otherwise pass URIs
    /// through (DD5 single-endpoint). When the decoded endpoint is outside
    /// the profile's allowed set, the read is rejected before dispatch.
    /// When the URI is not wrapped, the request is treated as a DD5 raw URI
    /// and dispatched to the sole in-profile active endpoint; if zero or
    /// more than one in-profile endpoint is active, the request is rejected.
    ///
    /// This always-strict decode is what closes the
    /// `(global>=2, profile==1)` foreign-endpoint bypass: without it, a
    /// `skip_wrap=true` decode would treat the wrapper as raw and dispatch
    /// to the profile's sole endpoint with a foreign endpoint name baked
    /// into the URI string.
    pub async fn route_resource_read(&self, wrapped_uri: &str) -> Result<Value, AdapterError> {
        // Strict-decode first. Successful decode → wrapper-style URI; the
        // resolved endpoint must be inside the profile.
        if let Ok((endpoint, original_uri)) = crate::resource_uri::decode_resource_uri(wrapped_uri)
        {
            if !self.allowed_endpoints.contains(&endpoint) {
                return Err(AdapterError::ProtocolError(format!(
                    "resource '{}' is not available in this profile",
                    wrapped_uri
                )));
            }
            return self
                .inner
                .dispatch_resource_read_to(&endpoint, &original_uri)
                .await;
        }

        // Not a wrapped URI → DD5 single-endpoint mode within the profile.
        // Require exactly one in-profile active endpoint; reject otherwise
        // because there is no endpoint hint to disambiguate the target.
        match self
            .inner
            .find_single_active_endpoint_in(&self.allowed_endpoints)
            .await
        {
            Some(endpoint) => {
                self.inner
                    .dispatch_resource_read_to(&endpoint, wrapped_uri)
                    .await
            }
            None => Err(AdapterError::ProtocolError(format!(
                "no active endpoint to serve resource '{}'",
                wrapped_uri
            ))),
        }
    }
}

#[async_trait]
impl MetaToolRegistry for ProfileRegistryView {
    async fn merged_catalog(&self) -> Vec<ToolInfo> {
        ProfileRegistryView::merged_catalog(self).await
    }

    async fn merged_catalog_with_lookup(
        &self,
    ) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        ProfileRegistryView::merged_catalog_with_lookup(self).await
    }

    async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: Value,
    ) -> Result<Value, AdapterError> {
        ProfileRegistryView::route_tool_call(self, prefixed_name, arguments).await
    }

    fn catalog_generation(&self) -> u64 {
        // Delegate to the underlying registry. A profile view is a strict
        // subset of the global catalog, so any mutation that bumps the
        // global generation also implicitly invalidates per-profile
        // search-index caches keyed on this counter.
        self.inner.catalog_generation()
    }

    fn subscribe_tools_changed(&self) -> broadcast::Receiver<String> {
        // Delegate to the underlying registry — R3.D's SSE handler is the
        // consumer that filters ticks against
        // [`Self::allowed_endpoints`].
        self.inner.subscribe_tools_changed()
    }
}

/// Default per-profile JS sandbox timeout, in seconds. Mirrors the value
/// passed to the global [`MetaToolHandler`] in `main.rs`.
const DEFAULT_SANDBOX_TIMEOUT_SECS: u64 = 30;

/// Relay-wide profile registry. Thread-safe, hot-reload-aware.
///
/// Profiles are keyed by their lowercased [`ProfileConfig::path`] — path
/// uniqueness is enforced case-insensitively at config-validation time (see
/// [`crate::config::validate_profiles`]) so lowercased keys yield the same
/// uniqueness invariant inside the registry.
pub struct ProfileRegistry {
    profiles: Arc<RwLock<HashMap<String, Arc<ProfileContext>>>>,
    adapter_registry: AdapterRegistry,
    /// Per-profile JS sandbox timeout passed through to each profile's
    /// [`MetaToolHandler`] at rebuild time. Held on the registry (not on
    /// each [`ProfileContext`]) so it stays in lockstep with the global
    /// handler's value and is reapplied on every hot reload.
    sandbox_timeout: Duration,
    /// Profile-membership broadcast — fan-out for
    /// `notifications/tools/list_changed` on `/mcp/{profile}/sse` streams
    /// when the *profile itself* changes (endpoint added/removed from the
    /// profile, profile created/deleted, or `js_execution` toggled, which
    /// flips whether `execute_tools` is advertised). Payload is the
    /// lowercased profile path; per-profile SSE handlers filter on it.
    ///
    /// This sits alongside [`AdapterRegistry`]'s per-endpoint
    /// `tools_changed_tx` broadcast rather than replacing it: per-endpoint
    /// ticks are still the right signal for profile streams when an
    /// endpoint *inside* the profile mutates, and keeping the two channels
    /// separate avoids reshaping every existing consumer of the relay-wide
    /// channel.
    profiles_changed_tx: broadcast::Sender<String>,
}

impl ProfileRegistry {
    /// Create an empty registry with the default sandbox timeout
    /// ([`DEFAULT_SANDBOX_TIMEOUT_SECS`]). Call [`Self::rebuild`] before
    /// serving any requests.
    pub fn new(adapter_registry: AdapterRegistry) -> Self {
        Self::with_sandbox_timeout(
            adapter_registry,
            Duration::from_secs(DEFAULT_SANDBOX_TIMEOUT_SECS),
        )
    }

    /// Create an empty registry with a custom sandbox timeout. Used by
    /// tests that want a tighter budget; production callers should use
    /// [`Self::new`] so the per-profile timeout matches the global
    /// `MetaToolHandler`.
    pub fn with_sandbox_timeout(
        adapter_registry: AdapterRegistry,
        sandbox_timeout: Duration,
    ) -> Self {
        let (profiles_changed_tx, _) = broadcast::channel(16);
        Self {
            profiles: Arc::new(RwLock::new(HashMap::new())),
            adapter_registry,
            sandbox_timeout,
            profiles_changed_tx,
        }
    }

    /// Subscribe to the profile-membership broadcast. Each `recv()` yields
    /// the lowercased profile path whose allowed-endpoints set or advertised
    /// tool surface (e.g. `js_execution` toggle) changed since the previous
    /// receive. `Lagged` is surfaced as a tick like the per-endpoint
    /// `subscribe_tools_changed` — consumers should forward unconditionally
    /// on lag.
    pub fn subscribe_profiles_changed(&self) -> broadcast::Receiver<String> {
        self.profiles_changed_tx.subscribe()
    }

    /// Send a synthetic profile-changed tick for the given profile path.
    /// Test-only escape hatch mirroring
    /// [`AdapterRegistry::tick_tools_changed_for_test`].
    #[cfg(test)]
    pub(crate) fn tick_profile_changed_for_test(&self, profile_path: &str) {
        let _ = self
            .profiles_changed_tx
            .send(profile_path.to_ascii_lowercase());
    }

    /// Replace the current profile set with one built from `profiles`.
    ///
    /// The swap is atomic from the perspective of [`Self::get`] / [`Self::list`]
    /// callers: a single write-lock acquisition replaces the entire map.
    /// Per-profile `js_execution` and `toon_output` are copied straight from
    /// the profile config — the loader (`validate_profiles`) requires those
    /// fields to be present, so there is no fallback to relay-wide defaults.
    ///
    /// After the swap, emits one
    /// [`subscribe_profiles_changed`](Self::subscribe_profiles_changed) tick
    /// per profile path whose membership or advertised-tool surface changed
    /// — added profiles, removed profiles, profiles whose allowed-endpoint
    /// set differs, and profiles whose `js_execution` toggle flipped (which
    /// adds/removes `execute_tools` from the advertised catalog). A pure
    /// `toon_output` flip is NOT signalled because it only affects response
    /// serialization, not the advertised tool list. No-op rebuilds (same
    /// configs in same order) emit nothing — satisfying spec acceptance #5
    /// for profile-scoped streams.
    pub async fn rebuild(&self, profiles: &[ProfileConfig]) {
        let mut new_map: HashMap<String, Arc<ProfileContext>> = HashMap::new();
        for profile in profiles {
            let allowed: HashSet<String> = profile.endpoints.iter().cloned().collect();
            let view = ProfileRegistryView::new(self.adapter_registry.clone(), allowed);
            // Per-profile `MetaToolHandler` parameterised over the filtered
            // view (locked decision Relay #2). The handler's search-index
            // cache is private to this context, so per-profile catalogs
            // never bleed into a different profile's search results.
            let handler = MetaToolHandler::new(Arc::new(view.clone()), self.sandbox_timeout);
            let ctx = ProfileContext {
                config: profile.clone(),
                registry_view: view,
                js_execution: profile.js_execution,
                toon_output: profile.toon_output,
                meta_tool_handler: Some(Arc::new(handler)),
            };
            new_map.insert(profile.path.to_ascii_lowercase(), Arc::new(ctx));
        }

        // Diff old vs new under the write lock so the membership ticks we
        // emit accurately describe the post-swap state. Collect the set of
        // changed paths first, then drop the lock before broadcasting so a
        // slow subscriber can't stall the swap.
        let changed: Vec<String> = {
            let mut guard = self.profiles.write().await;
            let mut changed = Vec::new();
            // Added or mutated profiles.
            for (path, new_ctx) in new_map.iter() {
                let dirty = match guard.get(path) {
                    None => true,
                    Some(old_ctx) => {
                        old_ctx.registry_view.allowed_endpoints()
                            != new_ctx.registry_view.allowed_endpoints()
                            || old_ctx.js_execution != new_ctx.js_execution
                    }
                };
                if dirty {
                    changed.push(path.clone());
                }
            }
            // Removed profiles.
            for path in guard.keys() {
                if !new_map.contains_key(path) {
                    changed.push(path.clone());
                }
            }
            *guard = new_map;
            changed
        };

        for path in changed {
            let _ = self.profiles_changed_tx.send(path);
        }
    }

    /// Look up a profile by URL path (case-insensitive).
    pub async fn get(&self, path: &str) -> Option<Arc<ProfileContext>> {
        self.profiles
            .read()
            .await
            .get(&path.to_ascii_lowercase())
            .cloned()
    }

    /// Snapshot all registered profile contexts. Order is unspecified —
    /// callers that present profiles to users should sort.
    pub async fn list(&self) -> Vec<Arc<ProfileContext>> {
        self.profiles.read().await.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
    use async_trait::async_trait;
    use serde_json::json;

    /// Minimal in-test adapter — mirrors `registry.rs`'s private MockAdapter.
    struct MockAdapter {
        tools: Vec<ToolInfo>,
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
        async fn read_resource(&self, uri: &str) -> Result<serde_json::Value, AdapterError> {
            // Echo the URI the adapter received so the profile-guard test
            // can assert (a) the reverse-rewrite stripped the wrapper and
            // (b) the call reached the correct endpoint.
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": format!("body for {uri}"),
                }]
            }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        fn server_type(&self) -> Option<String> {
            None
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    fn tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.into(),
            description: Some(format!("{name} tool")),
            input_schema: json!({"type": "object"}),
            annotations: None,
            ..Default::default()
        }
    }

    async fn registry_with_four_endpoints() -> AdapterRegistry {
        let registry = AdapterRegistry::new();
        for (name, tool_name) in [
            ("gmail", "send_email"),
            ("linear", "create_issue"),
            ("todoist", "add_task"),
            ("notes", "search_notes"),
        ] {
            registry
                .register(
                    name.into(),
                    Box::new(MockAdapter {
                        tools: vec![tool(tool_name)],
                    }),
                    "stdio".into(),
                    None,
                    Some(name.into()),
                )
                .await;
        }
        registry
    }

    /// Test-matrix row #7: profile with 2 of 4 endpoints → catalog has only
    /// those 2 endpoints' tools.
    #[tokio::test]
    async fn scoped_catalog_includes_only_profile_endpoints() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.expect("profile should exist");
        let catalog = ctx.registry_view.merged_catalog().await;
        let names: HashSet<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(catalog.len(), 2, "expected 2 tools, got {:?}", names);
        assert!(names.contains("gmail__send_email"));
        assert!(names.contains("linear__create_issue"));
        assert!(!names.contains("todoist__add_task"));
        assert!(!names.contains("notes__search_notes"));
    }

    /// Test-matrix row #8: call to a tool outside the profile → error.
    #[tokio::test]
    async fn route_tool_call_rejects_out_of_profile_tool() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let err = ctx
            .registry_view
            .route_tool_call("todoist__add_task", json!({}))
            .await
            .expect_err("call to out-of-profile tool must fail");
        match err {
            AdapterError::ProtocolError(msg) => {
                assert!(
                    msg.contains("todoist__add_task")
                        && msg.contains("not available in this profile"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected ProtocolError, got {other:?}"),
        }
    }

    /// Test-matrix row #9: call to a tool inside the profile → success.
    #[tokio::test]
    async fn route_tool_call_succeeds_for_in_profile_tool() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let result = ctx
            .registry_view
            .route_tool_call("gmail__send_email", json!({"to": "x@example.com"}))
            .await
            .expect("in-profile tool call should succeed");
        assert_eq!(result["called"], "send_email");
        assert_eq!(result["args"]["to"], "x@example.com");
    }

    /// §10.4 #28: a profile-scoped call whose arguments violate the tool's
    /// `inputSchema` is rejected by the same validation layer as the global
    /// path. The structured `isError: true` result is returned and the
    /// upstream adapter's `call_tool` is never invoked — proving validation
    /// runs identically through `ProfileRegistryView::route_tool_call`.
    #[tokio::test]
    async fn profile_scoped_call_validates_against_tool_schema() {
        let registry = AdapterRegistry::new();
        // Two endpoints so the merged catalog prefixes tool names, matching
        // how a real multi-server profile routes (`github__create_issue`).
        registry
            .register(
                "github".into(),
                Box::new(MockAdapter {
                    tools: vec![ToolInfo {
                        name: "create_issue".into(),
                        description: Some("create_issue tool".into()),
                        input_schema: json!({
                            "type": "object",
                            "properties": { "repo": { "type": "string" } },
                            "required": ["repo"],
                        }),
                        annotations: None,
                        ..Default::default()
                    }],
                }),
                "stdio".into(),
                None,
                Some("github".into()),
            )
            .await;
        registry
            .register(
                "gmail".into(),
                Box::new(MockAdapter {
                    tools: vec![tool("send_email")],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["github".into(), "gmail".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        // Missing the required `repo` field → validation must reject before
        // the call ever reaches the upstream adapter.
        let result = ctx
            .registry_view
            .route_tool_call("github__create_issue", json!({}))
            .await
            .expect("a validation failure returns Ok(isError), not Err");

        assert_eq!(
            result["isError"],
            json!(true),
            "profile-scoped bad args must return the structured validation result: {result}"
        );
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(
            text.contains("Input validation failed for tool 'github__create_issue'"),
            "validation message should name the prefixed profile-scoped tool: {text}"
        );
        assert!(
            result.get("called").is_none(),
            "upstream adapter call_tool must never run on a validation failure: {result}"
        );
    }

    #[tokio::test]
    async fn get_is_case_insensitive() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "Work-Stuff".into(),
            endpoints: vec![],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        assert!(pr.get("work-stuff").await.is_some());
        assert!(pr.get("WORK-STUFF").await.is_some());
        assert!(pr.get("Work-Stuff").await.is_some());
        assert!(pr.get("other").await.is_none());
    }

    #[tokio::test]
    async fn rebuild_swaps_atomically() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        pr.rebuild(&[ProfileConfig {
            name: "A".into(),
            path: "a".into(),
            endpoints: vec![],
            js_execution: false,
            toon_output: true,
        }])
        .await;
        assert!(pr.get("a").await.is_some());

        pr.rebuild(&[ProfileConfig {
            name: "B".into(),
            path: "b".into(),
            endpoints: vec![],
            js_execution: false,
            toon_output: true,
        }])
        .await;
        assert!(pr.get("a").await.is_none(), "old profile must be evicted");
        assert!(pr.get("b").await.is_some());
        assert_eq!(pr.list().await.len(), 1);
    }

    #[tokio::test]
    async fn rebuild_copies_js_and_toon_from_profile() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        pr.rebuild(&[
            ProfileConfig {
                name: "On".into(),
                path: "on".into(),
                endpoints: vec![],
                js_execution: true,
                toon_output: false,
            },
            ProfileConfig {
                name: "Off".into(),
                path: "off".into(),
                endpoints: vec![],
                js_execution: false,
                toon_output: true,
            },
        ])
        .await;

        let on = pr.get("on").await.unwrap();
        assert!(on.js_execution);
        assert!(!on.toon_output);

        let off = pr.get("off").await.unwrap();
        assert!(!off.js_execution);
        assert!(off.toon_output);
    }

    /// R3.A: every profile gets its own [`MetaToolHandler`] at rebuild
    /// time, wrapping the profile's [`ProfileRegistryView`]. The placeholder
    /// `None` from R2.A is gone.
    #[tokio::test]
    async fn rebuild_populates_per_profile_meta_tool_handler() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        pr.rebuild(&[ProfileConfig {
            name: "P".into(),
            path: "p".into(),
            endpoints: vec![],
            js_execution: false,
            toon_output: true,
        }])
        .await;
        assert!(pr.get("p").await.unwrap().meta_tool_handler.is_some());
    }

    /// Per-profile `MetaToolHandler::list_tools` sees only the profile's
    /// allowed-endpoint tools, not the global catalog.
    #[tokio::test]
    async fn per_profile_handler_scopes_list_tools() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let handler = ctx
            .meta_tool_handler
            .as_ref()
            .expect("R3.A populates the per-profile handler");
        let result = handler
            .list_tools(None, None)
            .await
            .expect("list_tools must succeed");
        let tools = result["tools"].as_array().expect("tools array");
        let names: HashSet<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            tools.len(),
            2,
            "expected 2 in-profile tools, got {:?}",
            names
        );
        assert!(names.contains("gmail__send_email"));
        assert!(names.contains("linear__create_issue"));
        assert!(!names.contains("todoist__add_task"));
        assert!(!names.contains("notes__search_notes"));
    }

    /// Test-matrix row #15: per-profile `MetaToolHandler::search_tools`
    /// only returns tools whose owning endpoint is in the profile's allowed
    /// set, even when the query would otherwise match a global,
    /// out-of-profile tool (`notes__search_notes` here scores on `"search"`
    /// but the profile view never surfaces it).
    #[tokio::test]
    async fn per_profile_handler_scopes_search_tools() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let handler = ctx
            .meta_tool_handler
            .as_ref()
            .expect("R3.A populates the per-profile handler");

        // Empty query → first-page slice of the profile-filtered catalog,
        // so the result is exactly the profile's two tools.
        let page = handler
            .search_tools("", None)
            .await
            .expect("search_tools (empty query) must succeed");
        let page_arr = page.as_array().expect("search result is a JSON array");
        let page_names: HashSet<&str> = page_arr
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            page_arr.len(),
            2,
            "empty-query page should match profile catalog size, got {:?}",
            page_names
        );
        assert!(page_names.contains("gmail__send_email"));
        assert!(page_names.contains("linear__create_issue"));
        assert!(!page_names.contains("todoist__add_task"));
        assert!(!page_names.contains("notes__search_notes"));

        // Token query → out-of-profile `notes__search_notes` would normally
        // match "search" but must not appear because the per-profile handler
        // never sees it.
        let matches = handler
            .search_tools("search", None)
            .await
            .expect("search_tools (token query) must succeed");
        let matches_arr = matches.as_array().expect("search result is an array");
        let match_names: HashSet<&str> = matches_arr
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert!(
            !match_names.contains("notes__search_notes"),
            "out-of-profile tool must not appear in per-profile search, got {:?}",
            match_names
        );
        assert!(
            !match_names.contains("todoist__add_task"),
            "out-of-profile tool must not appear in per-profile search, got {:?}",
            match_names
        );
    }

    /// Test-matrix row #12: an `execute_tools` script run by the per-profile
    /// [`MetaToolHandler`] cannot reach a tool whose owning endpoint is
    /// outside the profile. The sandbox sees only the profile-filtered
    /// catalog, so `call("todoist__add_task")` from inside the script
    /// surfaces the "unknown tool" error rather than reaching the global
    /// registry.
    #[tokio::test]
    async fn per_profile_handler_execute_tools_rejects_out_of_profile_call() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::with_sandbox_timeout(registry, Duration::from_secs(5));
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let handler = ctx
            .meta_tool_handler
            .as_ref()
            .expect("R3.A populates the per-profile handler");

        // Wrap the call so the script surfaces the underlying JS error
        // string from `tools.call` rather than propagating it out as a
        // sandbox-level error.
        let script = r#"
            try {
                call("todoist__add_task", { content: "x" });
                return { ok: true };
            } catch (e) {
                return { error: String(e && e.message ? e.message : e) };
            }
        "#;
        let result = handler
            .execute_tools(script, "", "")
            .await
            .expect("execute_tools should return the script's error value");
        let err_msg = result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            err_msg.contains("todoist__add_task"),
            "expected error to mention the out-of-profile tool, got: {err_msg}"
        );
    }

    // ---- T4 — profile-scoped `route_resource_read` -----------------------

    /// A wrapped URI whose owning endpoint is inside the profile dispatches
    /// successfully, and the adapter receives the *original* (un-wrapped) URI.
    #[tokio::test]
    async fn route_resource_read_succeeds_for_in_profile_endpoint() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let wrapped = crate::resource_uri::encode_resource_uri("gmail", "ui://inbox/123");
        let result = ctx
            .registry_view
            .route_resource_read(&wrapped)
            .await
            .expect("in-profile resource read must succeed");
        assert_eq!(
            result["contents"][0]["uri"], "ui://inbox/123",
            "wrapper must be stripped before reaching the adapter"
        );
    }

    /// A wrapped URI whose owning endpoint is outside the profile is rejected
    /// before delegation, mirroring the `route_tool_call` guard.
    #[tokio::test]
    async fn route_resource_read_rejects_out_of_profile_endpoint() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Work".into(),
            path: "work".into(),
            endpoints: vec!["gmail".into(), "linear".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("work").await.unwrap();
        let wrapped = crate::resource_uri::encode_resource_uri("todoist", "ui://tasks/today");
        let err = ctx
            .registry_view
            .route_resource_read(&wrapped)
            .await
            .expect_err("out-of-profile read must be rejected");
        match err {
            AdapterError::ProtocolError(msg) => {
                assert!(
                    msg.contains("not available in this profile"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected ProtocolError, got {other:?}"),
        }
    }

    /// T12 regression — the `(global>=2, profile==1)` foreign-endpoint
    /// bypass. With multiple endpoints globally but only one in the
    /// profile, the profile's in-scope `skip_wrap` is `true` so the
    /// incoming URI is treated as its own original. A wrapped URI that
    /// targets a *foreign* endpoint must NOT be accepted as a raw URI by
    /// the profile's single allowed endpoint — the dispatch target is
    /// resolved against the profile's allowed set, not the global registry.
    #[tokio::test]
    async fn route_resource_read_rejects_wrapped_foreign_uri_in_single_endpoint_profile() {
        let registry = registry_with_four_endpoints().await;
        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Solo".into(),
            path: "solo".into(),
            endpoints: vec!["gmail".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("solo").await.unwrap();
        // Profile has 1 endpoint → its `skip_wrap` is true; global has 4
        // → wrapping is normally in effect. Build a fully-wrapped URI
        // targeting a foreign endpoint and submit it through the
        // profile-scoped path.
        let wrapped = crate::resource_uri::encode_resource_uri("todoist", "ui://tasks/today");
        let result = ctx.registry_view.route_resource_read(&wrapped).await;
        match result {
            Ok(v) => panic!(
                "wrapped foreign-endpoint URI must NOT be served by the profile's \
                 sole endpoint (bypass!): {v}"
            ),
            Err(AdapterError::ProtocolError(_)) => {
                // Either path is acceptable as a closed bypass: the URI
                // may be reported as not-available-in-profile, or as
                // unknown by the adapter (the in-profile endpoint won't
                // recognise the `mcp-relay://` scheme). The critical
                // invariant is that we never see the foreign adapter's
                // `read_resource` echo for `ui://tasks/today`.
            }
            Err(other) => panic!("expected ProtocolError, got {other:?}"),
        }
    }

    /// T12 regression (list side) — the outbound `list_resources`
    /// aggregation must also use the profile-local active count when
    /// deciding to wrap. With one in-profile endpoint, URIs flow through
    /// unwrapped, matching the unwrap trigger used by
    /// [`ProfileRegistryView::route_resource_read`]. The list must contain
    /// only the profile's endpoint's resources.
    #[tokio::test]
    async fn list_resources_is_profile_scoped_with_in_profile_active_count() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "gmail".into(),
                Box::new(ResourceMockForProfile {
                    resources: vec![json!({ "uri": "ui://inbox/1", "name": "inbox" })],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        registry
            .register(
                "todoist".into(),
                Box::new(ResourceMockForProfile {
                    resources: vec![json!({ "uri": "ui://tasks/today", "name": "today" })],
                }),
                "stdio".into(),
                None,
                Some("todoist".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Solo".into(),
            path: "solo".into(),
            endpoints: vec!["gmail".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("solo").await.unwrap();
        let resources = ctx.registry_view.list_resources().await;
        assert_eq!(
            resources.len(),
            1,
            "must list only gmail's resources: {resources:?}"
        );
        // One in-profile endpoint → wrap is skipped, the URI is its raw
        // original (no `mcp-relay://` prefix). This is exactly what
        // `route_resource_read`'s in-profile single-endpoint dispatch
        // expects, so the listing and read paths stay symmetric.
        assert_eq!(resources[0]["uri"], "ui://inbox/1");
    }

    /// T9 — profile-scoped `list_prompts` filters to the profile's allowed
    /// endpoints and applies the *in-profile* active count when deciding to
    /// prefix names. With one in-profile endpoint, names pass through
    /// unprefixed (DD5 parity); foreign endpoints' prompts are excluded.
    #[tokio::test]
    async fn list_prompts_is_profile_scoped_with_in_profile_active_count() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "gmail".into(),
                Box::new(PromptMockForProfile {
                    prompts: vec![json!({ "name": "compose", "description": "Compose mail" })],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        registry
            .register(
                "todoist".into(),
                Box::new(PromptMockForProfile {
                    prompts: vec![json!({ "name": "add_task", "description": "Add a task" })],
                }),
                "stdio".into(),
                None,
                Some("todoist".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Solo".into(),
            path: "solo".into(),
            endpoints: vec!["gmail".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("solo").await.unwrap();
        let prompts = ctx.registry_view.list_prompts().await;
        assert_eq!(
            prompts.len(),
            1,
            "must list only gmail's prompts: {prompts:?}"
        );
        // One in-profile endpoint → prefix is skipped, the name is its raw
        // original. Symmetric with the registry's DD5 unprefixed mode.
        assert_eq!(prompts[0]["name"], "compose");
    }

    /// T9 — profile-scoped `list_prompts` prefixes names when the in-profile
    /// active count is ≥ 2, even when only a subset of the global registry
    /// is in the profile.
    #[tokio::test]
    async fn list_prompts_profile_prefixes_when_multiple_in_profile() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "gmail".into(),
                Box::new(PromptMockForProfile {
                    prompts: vec![json!({ "name": "compose" })],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        registry
            .register(
                "todoist".into(),
                Box::new(PromptMockForProfile {
                    prompts: vec![json!({ "name": "add_task" })],
                }),
                "stdio".into(),
                None,
                Some("todoist".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "All".into(),
            path: "all".into(),
            endpoints: vec!["gmail".into(), "todoist".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("all").await.unwrap();
        let mut prompts = ctx.registry_view.list_prompts().await;
        prompts.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0]["name"], "gmail__compose");
        assert_eq!(prompts[1]["name"], "todoist__add_task");
    }

    // ---- T10 — profile-scoped `route_prompt_get` --------------------------

    /// Adapter mock that advertises a prompt and serves `prompts/get` with
    /// content blocks the slot #9 wrapper must visit. Tags responses with
    /// `endpoint_label` so the profile-scoped dispatch can be verified.
    struct PromptGetMockForProfile {
        endpoint_label: String,
        prompts: Vec<serde_json::Value>,
    }

    #[async_trait]
    impl McpAdapter for PromptGetMockForProfile {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({}))
        }
        async fn list_prompts(&self) -> Result<Vec<serde_json::Value>, AdapterError> {
            Ok(self.prompts.clone())
        }
        async fn get_prompt(
            &self,
            name: &str,
            _arguments: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({
                "endpoint": self.endpoint_label,
                "raw_name": name,
                "messages": [{
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "see https://example.com" },
                        { "type": "resource_link", "uri": "ui://app/link", "name": "Open" }
                    ]
                }]
            }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    /// Profile-scoped `route_prompt_get` dispatches in-profile prompts to
    /// the owning upstream, applies slot #9 URI wrapping using the
    /// *in-profile* active count, and forwards the raw name verbatim.
    #[tokio::test]
    async fn route_prompt_get_succeeds_for_in_profile_endpoint() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "gmail".into(),
                Box::new(PromptGetMockForProfile {
                    endpoint_label: "gmail".into(),
                    prompts: vec![json!({ "name": "compose" })],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        registry
            .register(
                "todoist".into(),
                Box::new(PromptGetMockForProfile {
                    endpoint_label: "todoist".into(),
                    prompts: vec![json!({ "name": "add_task" })],
                }),
                "stdio".into(),
                None,
                Some("todoist".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "All".into(),
            path: "all".into(),
            endpoints: vec!["gmail".into(), "todoist".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("all").await.unwrap();
        let result = ctx
            .registry_view
            .route_prompt_get("gmail__compose", None)
            .await
            .expect("in-profile prompt must dispatch");
        assert_eq!(result["endpoint"], "gmail");
        assert_eq!(result["raw_name"], "compose");
        // Multi-endpoint profile → slot #9 wraps the resource_link URI.
        let link_uri = result["messages"][0]["content"][1]["uri"].as_str().unwrap();
        let (ep, orig) = crate::resource_uri::decode_resource_uri(link_uri).unwrap();
        assert_eq!(ep, "gmail");
        assert_eq!(orig, "ui://app/link");
    }

    /// Profile-scoped `route_prompt_get` rejects a prompt whose owning
    /// endpoint is not in the profile's allowed set — even when that
    /// endpoint exists in the global registry. This is the prompts-side
    /// analogue of the foreign-endpoint guard `route_resource_read` /
    /// `route_tool_call` enforce.
    #[tokio::test]
    async fn route_prompt_get_rejects_out_of_profile_endpoint() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "gmail".into(),
                Box::new(PromptGetMockForProfile {
                    endpoint_label: "gmail".into(),
                    prompts: vec![json!({ "name": "compose" })],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        registry
            .register(
                "todoist".into(),
                Box::new(PromptGetMockForProfile {
                    endpoint_label: "todoist".into(),
                    prompts: vec![json!({ "name": "add_task" })],
                }),
                "stdio".into(),
                None,
                Some("todoist".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Solo".into(),
            path: "solo".into(),
            endpoints: vec!["gmail".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("solo").await.unwrap();
        // `todoist__add_task` exists in the global registry but is not in
        // the profile's allowed set; the lookup is built from the in-profile
        // list so the name doesn't even appear in `lookup`.
        let err = ctx
            .registry_view
            .route_prompt_get("todoist__add_task", None)
            .await
            .expect_err("foreign prompt must reject");
        match err {
            AdapterError::ProtocolError(msg) => {
                assert!(
                    msg.contains("no prompt") || msg.contains("not available in this profile"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected ProtocolError, got: {other:?}"),
        }
    }

    /// Profile-scoped `route_prompt_get` follows the in-profile DD5 rule:
    /// when the profile has exactly one active endpoint, names pass
    /// through unprefixed and slot #9 URIs flow through unwrapped, even
    /// when the global registry has multiple endpoints.
    #[tokio::test]
    async fn route_prompt_get_single_in_profile_passes_through() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "gmail".into(),
                Box::new(PromptGetMockForProfile {
                    endpoint_label: "gmail".into(),
                    prompts: vec![json!({ "name": "compose" })],
                }),
                "stdio".into(),
                None,
                Some("gmail".into()),
            )
            .await;
        registry
            .register(
                "todoist".into(),
                Box::new(PromptGetMockForProfile {
                    endpoint_label: "todoist".into(),
                    prompts: vec![json!({ "name": "add_task" })],
                }),
                "stdio".into(),
                None,
                Some("todoist".into()),
            )
            .await;

        let pr = ProfileRegistry::new(registry);
        pr.rebuild(&[ProfileConfig {
            name: "Solo".into(),
            path: "solo".into(),
            endpoints: vec!["gmail".into()],
            js_execution: false,
            toon_output: true,
        }])
        .await;

        let ctx = pr.get("solo").await.unwrap();
        let result = ctx
            .registry_view
            .route_prompt_get("compose", None)
            .await
            .expect("DD5 in-profile dispatch must succeed");
        assert_eq!(result["endpoint"], "gmail");
        assert_eq!(result["raw_name"], "compose");
        // In-profile active_count == 1 → slot #9 wrap is skipped.
        assert_eq!(result["messages"][0]["content"][1]["uri"], "ui://app/link");
    }

    /// Adapter mock for profile-scoped resource listing tests. Kept local
    /// to the module so it doesn't grow the existing `MockAdapter` with
    /// resource fields used only by these tests.
    struct ResourceMockForProfile {
        resources: Vec<serde_json::Value>,
    }

    #[async_trait]
    impl McpAdapter for ResourceMockForProfile {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            name: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        async fn list_resources(&self) -> Result<Vec<serde_json::Value>, AdapterError> {
            Ok(self.resources.clone())
        }
        async fn read_resource(&self, uri: &str) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": format!("body for {uri}"),
                }]
            }))
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    /// Adapter mock for profile-scoped prompt listing tests. Kept local
    /// to the module so it doesn't grow the existing mocks with prompt
    /// fields used only by these tests.
    struct PromptMockForProfile {
        prompts: Vec<serde_json::Value>,
    }

    #[async_trait]
    impl McpAdapter for PromptMockForProfile {
        async fn initialize(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn list_tools(&self) -> Result<Vec<ToolInfo>, AdapterError> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            name: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, AdapterError> {
            Ok(json!({ "called": name, "args": arguments }))
        }
        async fn list_prompts(&self) -> Result<Vec<serde_json::Value>, AdapterError> {
            Ok(self.prompts.clone())
        }
        fn health(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }
}
