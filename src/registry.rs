use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::prefix;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// A registered adapter with its metadata.
pub struct RegisteredAdapter {
    pub adapter: Box<dyn McpAdapter>,
    pub transport: String,
    pub description: Option<String>,
    pub tool_prefix: Option<String>,
    pub last_activity: Option<Instant>,
    pub disabled: bool,
    pub disabled_tools: HashSet<String>,
}

/// Cached catalog type: `(tool_list, reverse_lookup_map)`.
type CatalogCache = (Vec<ToolInfo>, HashMap<String, (String, String)>);

/// Thread-safe registry of MCP adapters keyed by endpoint name.
#[derive(Clone)]
pub struct AdapterRegistry {
    adapters: Arc<RwLock<HashMap<String, RegisteredAdapter>>>,
    catalog_cache: Arc<RwLock<Option<CatalogCache>>>,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterRegistry {
    /// Create a new, empty adapter registry.
    pub fn new() -> Self {
        Self {
            adapters: Arc::new(RwLock::new(HashMap::new())),
            catalog_cache: Arc::new(RwLock::new(None)),
        }
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
        self.adapters.write().await.insert(
            name,
            RegisteredAdapter {
                adapter,
                transport,
                description,
                tool_prefix,
                last_activity: None,
                disabled: false,
                disabled_tools: HashSet::new(),
            },
        );
        self.invalidate_catalog_cache().await;
    }

    /// Remove an adapter by endpoint name.
    pub async fn remove(&self, name: &str) -> Option<RegisteredAdapter> {
        let result = self.adapters.write().await.remove(name);
        if result.is_some() {
            self.invalidate_catalog_cache().await;
        }
        result
    }

    /// Invalidate the cached catalog so the next read rebuilds it.
    pub async fn invalidate_catalog_cache(&self) {
        *self.catalog_cache.write().await = None;
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

            match entry.adapter.list_tools().await {
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
    /// target endpoint and raw tool name for the given prefixed name.
    pub async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        let (_, lookup) = self.merged_catalog_with_lookup().await;

        let (endpoint, tool) = lookup.get(prefixed_name).ok_or_else(|| {
            AdapterError::ProtocolError(format!(
                "no tool found for prefixed name '{}'",
                prefixed_name
            ))
        })?;

        let adapters = self.adapters.read().await;
        let entry = adapters.get(endpoint).ok_or_else(|| {
            AdapterError::ProtocolError(format!("no adapter found for endpoint '{}'", endpoint))
        })?;

        if entry.disabled {
            return Err(AdapterError::ProtocolError(format!(
                "endpoint '{}' is disabled",
                endpoint
            )));
        }

        if !matches!(entry.adapter.health(), HealthStatus::Healthy) {
            return Err(AdapterError::ProtocolError(format!(
                "tool '{}' is currently unavailable: endpoint '{}' is not healthy",
                tool, endpoint
            )));
        }

        if entry.disabled_tools.contains(tool) {
            return Err(AdapterError::ProtocolError(format!(
                "tool '{}' is disabled on endpoint '{}'",
                tool, endpoint
            )));
        }

        entry.adapter.call_tool(tool, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::StartingAdapter;
    use async_trait::async_trait;
    use serde_json::json;

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
}
