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
        Self {
            profiles: Arc::new(RwLock::new(HashMap::new())),
            adapter_registry,
            sandbox_timeout,
        }
    }

    /// Replace the current profile set with one built from `profiles`.
    ///
    /// The swap is atomic from the perspective of [`Self::get`] / [`Self::list`]
    /// callers: a single write-lock acquisition replaces the entire map.
    /// Per-profile `js_execution` and `toon_output` are copied straight from
    /// the profile config — the loader (`validate_profiles`) requires those
    /// fields to be present, so there is no fallback to relay-wide defaults.
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

        *self.profiles.write().await = new_map;
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
            .execute_tools(script)
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
}
