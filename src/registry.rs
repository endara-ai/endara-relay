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
    pub last_activity: Option<Instant>,
    pub disabled: bool,
    pub disabled_tools: HashSet<String>,
}

/// Thread-safe registry of MCP adapters keyed by endpoint name.
#[derive(Clone)]
pub struct AdapterRegistry {
    adapters: Arc<RwLock<HashMap<String, RegisteredAdapter>>>,
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
        }
    }

    /// Register an adapter under the given endpoint name.
    pub async fn register(
        &self,
        name: String,
        adapter: Box<dyn McpAdapter>,
        transport: String,
        description: Option<String>,
    ) {
        debug!(endpoint = %name, "Registering adapter");
        self.adapters.write().await.insert(
            name,
            RegisteredAdapter {
                adapter,
                transport,
                description,
                last_activity: None,
                disabled: false,
                disabled_tools: HashSet::new(),
            },
        );
    }

    /// Remove an adapter by endpoint name.
    pub async fn remove(&self, name: &str) -> Option<RegisteredAdapter> {
        self.adapters.write().await.remove(name)
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

    /// Build merged tool catalog from all adapters, with collision-aware prefixed names.
    ///
    /// Naming strategy:
    /// 1. Each adapter reports a `server_type()` (sanitized `serverInfo.name`).
    ///    If unavailable, the endpoint name is used as fallback.
    /// 2. If only one endpoint has a given server_type, tools are named
    ///    `{server_type}__{tool}`.
    /// 3. If multiple endpoints share a server_type (collision), tools are named
    ///    `{server_type}__{endpoint_name}__{tool}`.
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
    pub async fn merged_catalog_with_lookup(
        &self,
    ) -> (Vec<ToolInfo>, HashMap<String, (String, String)>) {
        let adapters = self.adapters.read().await;

        // First pass: determine server_type for each endpoint and detect collisions
        let mut server_type_map: HashMap<String, String> = HashMap::new(); // endpoint -> server_type
        let mut type_counts: HashMap<String, usize> = HashMap::new();

        for (endpoint_name, entry) in adapters.iter() {
            if entry.disabled {
                continue;
            }
            let st = entry
                .adapter
                .server_type()
                .unwrap_or_else(|| endpoint_name.clone());
            *type_counts.entry(st.clone()).or_insert(0) += 1;
            server_type_map.insert(endpoint_name.clone(), st);
        }

        // Second pass: build catalog with collision-aware names
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

            let server_type = server_type_map
                .get(endpoint_name)
                .cloned()
                .unwrap_or_else(|| endpoint_name.clone());
            let has_collision = type_counts.get(&server_type).copied().unwrap_or(0) > 1;

            match entry.adapter.list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        if entry.disabled_tools.contains(&tool.name) {
                            continue;
                        }
                        let instance = if has_collision {
                            Some(endpoint_name.as_str())
                        } else {
                            None
                        };
                        let prefixed_name =
                            prefix::encode_tool_name(&server_type, instance, &tool.name);
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
                            prefixed_name.clone(),
                            (endpoint_name.clone(), tool.name.clone()),
                        );
                        catalog.push(ToolInfo {
                            name: prefixed_name,
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

    // --- No collision: each endpoint has unique server_type (falls back to endpoint name) ---

    #[tokio::test]
    async fn test_merged_catalog_no_collision() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep1".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
                None,
            )
            .await;
        registry
            .register(
                "ep2".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("write")])),
                "stdio".into(),
                None,
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        // No collision: server_type == endpoint_name, so format is {ep}__{tool}
        assert!(names.contains(&"ep1__read"));
        assert!(names.contains(&"ep2__write"));
    }

    // --- With server_type: no collision ---

    #[tokio::test]
    async fn test_merged_catalog_with_server_type_no_collision() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "echo-ep".into(),
                Box::new(MockAdapter::healthy_with_type(vec![make_tool("echo")], "echo_mcp")),
                "stdio".into(),
                None,
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "echo_mcp__echo");
    }

    // --- With server_type: collision (same type, different endpoints) ---

    #[tokio::test]
    async fn test_merged_catalog_collision_adds_instance() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "fs-local".into(),
                Box::new(MockAdapter::healthy_with_type(vec![make_tool("read")], "filesystem")),
                "stdio".into(),
                None,
            )
            .await;
        registry
            .register(
                "fs-remote".into(),
                Box::new(MockAdapter::healthy_with_type(vec![make_tool("read")], "filesystem")),
                "stdio".into(),
                None,
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        // Collision: both have server_type "filesystem", so instance prefix added
        assert!(names.contains(&"filesystem__fs-local__read"));
        assert!(names.contains(&"filesystem__fs-remote__read"));
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
            )
            .await;
        registry
            .register(
                "bad".into(),
                Box::new(MockAdapter::unhealthy()),
                "stdio".into(),
                None,
            )
            .await;

        let catalog = registry.merged_catalog().await;
        // unhealthy with no tools → only 1 tool in catalog
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "good__tool");
    }

    // --- Route tool call ---

    #[tokio::test]
    async fn test_route_tool_call() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("echo")])),
                "stdio".into(),
                None,
            )
            .await;

        let result = registry
            .route_tool_call("ep__echo", json!({"msg": "hi"}))
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["msg"], "hi");
    }

    #[tokio::test]
    async fn test_route_tool_call_with_server_type() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "echo-ep".into(),
                Box::new(MockAdapter::healthy_with_type(vec![make_tool("echo")], "echo_mcp")),
                "stdio".into(),
                None,
            )
            .await;

        let result = registry
            .route_tool_call("echo_mcp__echo", json!({"msg": "hi"}))
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
    }

    #[tokio::test]
    async fn test_route_tool_call_with_collision() {
        let registry = AdapterRegistry::new();
        registry
            .register(
                "fs-local".into(),
                Box::new(MockAdapter::healthy_with_type(vec![make_tool("read")], "fs")),
                "stdio".into(),
                None,
            )
            .await;
        registry
            .register(
                "fs-remote".into(),
                Box::new(MockAdapter::healthy_with_type(vec![make_tool("read")], "fs")),
                "stdio".into(),
                None,
            )
            .await;

        let result = registry
            .route_tool_call("fs__fs-local__read", json!({}))
            .await
            .unwrap();
        assert_eq!(result["called"], "read");

        let result = registry
            .route_tool_call("fs__fs-remote__read", json!({}))
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
            )
            .await;
        registry
            .register(
                "bad".into(),
                Box::new(MockAdapter::unhealthy()),
                "stdio".into(),
                None,
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
            )
            .await;
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("new_tool")])),
                "sse".into(),
                None,
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "ep__new_tool");
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
            )
            .await;

        let result = registry.route_tool_call("sick__tool", json!({})).await;
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
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 3);
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"ep__read"));
        assert!(names.contains(&"ep__write"));
        assert!(names.contains(&"ep__delete"));
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

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "ep__write");
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
        let result = registry.route_tool_call("ep__t", json!({})).await;
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
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let read_tool = catalog.iter().find(|t| t.name == "broken__read").unwrap();
        assert_eq!(
            read_tool.description.as_deref(),
            Some("[⚠️ UNAVAILABLE] [broken] read tool")
        );

        let write_tool = catalog.iter().find(|t| t.name == "broken__write").unwrap();
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
            )
            .await;

        let result = registry
            .route_tool_call("broken__read", json!({}))
            .await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("unavailable"));
        assert!(err_msg.contains("broken"));
    }

    // --- Collision: multiple endpoints, no server_type override → endpoint name used ---
    // Different endpoint names = no collision = no instance prefix

    #[tokio::test]
    async fn test_multiple_endpoints_different_names_no_collision() {
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
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 4);

        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        // Different endpoint names → no collision → {endpoint}__{tool}
        assert!(names.contains(&"fs-local__read"));
        assert!(names.contains(&"fs-local__write"));
        assert!(names.contains(&"fs-remote__read"));
        assert!(names.contains(&"fs-remote__write"));

        let local_read = catalog
            .iter()
            .find(|t| t.name == "fs-local__read")
            .unwrap();
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
            )
            .await;
        registry
            .register(
                "fs-down".into(),
                Box::new(MockAdapter::unhealthy_with_tools(vec![make_tool("read")])),
                "stdio".into(),
                None,
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let ok_tool = catalog
            .iter()
            .find(|t| t.name == "fs-ok__read")
            .unwrap();
        assert_eq!(ok_tool.description.as_deref(), Some("[fs-ok] read tool"));

        let down_tool = catalog
            .iter()
            .find(|t| t.name == "fs-down__read")
            .unwrap();
        assert_eq!(
            down_tool.description.as_deref(),
            Some("[⚠️ UNAVAILABLE] [fs-down] read tool")
        );
    }
}
