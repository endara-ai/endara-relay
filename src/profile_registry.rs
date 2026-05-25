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

use serde_json::Value;
use tokio::sync::RwLock;

use crate::adapter::{AdapterError, ToolInfo};
use crate::config::{ProfileConfig, RelayConfig};
use crate::js_sandbox::MetaToolHandler;
use crate::registry::AdapterRegistry;

/// Effective default for [`RelayConfig::local_js_execution`] when the field
/// is omitted from `config.toml`. Mirrors the main.rs startup default.
const DEFAULT_LOCAL_JS_EXECUTION: bool = false;

/// Effective default for [`RelayConfig::toon_output`] when neither
/// `--no-toon` nor the config field is set. Mirrors the main.rs startup
/// default.
const DEFAULT_TOON_OUTPUT: bool = true;

/// Runtime state for a single resolved profile.
///
/// Constructed by [`ProfileRegistry::rebuild`] and handed (as `Arc`) to the
/// profile-scoped request handlers landing in R2.B. Per-profile JS-execution
/// and TOON-output flags are pre-resolved here so handlers don't have to
/// reach back into [`RelayConfig`].
pub struct ProfileContext {
    /// The original profile configuration this context was built from.
    pub config: ProfileConfig,
    /// Filtered view over the global [`AdapterRegistry`].
    pub registry_view: ProfileRegistryView,
    /// Resolved per-profile JS-execution flag (inherited from
    /// `RelayConfig::local_js_execution` when [`ProfileConfig::js_execution`]
    /// is `None`).
    pub js_execution: bool,
    /// Resolved per-profile TOON-output flag (inherited from
    /// `RelayConfig::toon_output` when [`ProfileConfig::toon_output`] is
    /// `None`).
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

/// Relay-wide profile registry. Thread-safe, hot-reload-aware.
///
/// Profiles are keyed by their lowercased [`ProfileConfig::path`] — path
/// uniqueness is enforced case-insensitively at config-validation time (see
/// [`crate::config::validate_profiles`]) so lowercased keys yield the same
/// uniqueness invariant inside the registry.
pub struct ProfileRegistry {
    profiles: Arc<RwLock<HashMap<String, Arc<ProfileContext>>>>,
    adapter_registry: AdapterRegistry,
}

impl ProfileRegistry {
    /// Create an empty registry. Call [`Self::rebuild`] before serving any
    /// requests.
    pub fn new(adapter_registry: AdapterRegistry) -> Self {
        Self {
            profiles: Arc::new(RwLock::new(HashMap::new())),
            adapter_registry,
        }
    }

    /// Replace the current profile set with one built from `profiles`.
    ///
    /// The swap is atomic from the perspective of [`Self::get`] / [`Self::list`]
    /// callers: a single write-lock acquisition replaces the entire map.
    /// Resolves per-profile `js_execution` and `toon_output` against
    /// `relay_config`, falling back to the same defaults `main.rs` uses at
    /// startup (`local_js_execution = false`, `toon_output = true`).
    pub async fn rebuild(&self, profiles: &[ProfileConfig], relay_config: &RelayConfig) {
        let global_js = relay_config
            .local_js_execution
            .unwrap_or(DEFAULT_LOCAL_JS_EXECUTION);
        let global_toon = relay_config.toon_output.unwrap_or(DEFAULT_TOON_OUTPUT);

        let mut new_map: HashMap<String, Arc<ProfileContext>> = HashMap::new();
        for profile in profiles {
            let allowed: HashSet<String> = profile.endpoints.iter().cloned().collect();
            let view = ProfileRegistryView::new(self.adapter_registry.clone(), allowed);
            let ctx = ProfileContext {
                config: profile.clone(),
                registry_view: view,
                js_execution: profile.js_execution.unwrap_or(global_js),
                toon_output: profile.toon_output.unwrap_or(global_toon),
                meta_tool_handler: None,
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

    fn relay_cfg() -> RelayConfig {
        RelayConfig {
            machine_name: "test".into(),
            local_js_execution: None,
            token_dir: None,
            allow_insecure_oauth: None,
            toon_output: None,
            startup_init_timeout_secs: None,
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
        pr.rebuild(
            &[ProfileConfig {
                name: "Work".into(),
                path: "work".into(),
                endpoints: vec!["gmail".into(), "linear".into()],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
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
        pr.rebuild(
            &[ProfileConfig {
                name: "Work".into(),
                path: "work".into(),
                endpoints: vec!["gmail".into(), "linear".into()],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
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
        pr.rebuild(
            &[ProfileConfig {
                name: "Work".into(),
                path: "work".into(),
                endpoints: vec!["gmail".into(), "linear".into()],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
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
        pr.rebuild(
            &[ProfileConfig {
                name: "Work".into(),
                path: "Work-Stuff".into(),
                endpoints: vec![],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
        .await;

        assert!(pr.get("work-stuff").await.is_some());
        assert!(pr.get("WORK-STUFF").await.is_some());
        assert!(pr.get("Work-Stuff").await.is_some());
        assert!(pr.get("other").await.is_none());
    }

    #[tokio::test]
    async fn rebuild_swaps_atomically() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        pr.rebuild(
            &[ProfileConfig {
                name: "A".into(),
                path: "a".into(),
                endpoints: vec![],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
        .await;
        assert!(pr.get("a").await.is_some());

        pr.rebuild(
            &[ProfileConfig {
                name: "B".into(),
                path: "b".into(),
                endpoints: vec![],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
        .await;
        assert!(pr.get("a").await.is_none(), "old profile must be evicted");
        assert!(pr.get("b").await.is_some());
        assert_eq!(pr.list().await.len(), 1);
    }

    #[tokio::test]
    async fn resolves_js_and_toon_inheritance() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        let relay = RelayConfig {
            machine_name: "test".into(),
            local_js_execution: Some(true),
            token_dir: None,
            allow_insecure_oauth: None,
            toon_output: Some(false),
            startup_init_timeout_secs: None,
        };
        pr.rebuild(
            &[
                ProfileConfig {
                    name: "Inherit".into(),
                    path: "inherit".into(),
                    endpoints: vec![],
                    js_execution: None,
                    toon_output: None,
                },
                ProfileConfig {
                    name: "Override".into(),
                    path: "override".into(),
                    endpoints: vec![],
                    js_execution: Some(false),
                    toon_output: Some(true),
                },
            ],
            &relay,
        )
        .await;

        let inherit = pr.get("inherit").await.unwrap();
        assert!(inherit.js_execution, "should inherit relay's true");
        assert!(!inherit.toon_output, "should inherit relay's false");

        let override_ctx = pr.get("override").await.unwrap();
        assert!(!override_ctx.js_execution);
        assert!(override_ctx.toon_output);
    }

    #[tokio::test]
    async fn meta_tool_handler_is_none_until_r3a() {
        let pr = ProfileRegistry::new(AdapterRegistry::new());
        pr.rebuild(
            &[ProfileConfig {
                name: "P".into(),
                path: "p".into(),
                endpoints: vec![],
                js_execution: None,
                toon_output: None,
            }],
            &relay_cfg(),
        )
        .await;
        assert!(pr.get("p").await.unwrap().meta_tool_handler.is_none());
    }
}
