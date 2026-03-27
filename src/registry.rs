use crate::adapter::{AdapterError, HealthStatus, McpAdapter, ToolInfo};
use crate::prefix;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// A registered adapter with its metadata.
pub struct RegisteredAdapter {
    pub adapter: Box<dyn McpAdapter>,
    pub transport: String,
    pub last_activity: Option<Instant>,
    pub stderr_lines: Arc<RwLock<Vec<String>>>,
}

/// Thread-safe registry of MCP adapters keyed by endpoint name.
#[derive(Clone)]
pub struct AdapterRegistry {
    machine_name: String,
    adapters: Arc<RwLock<HashMap<String, RegisteredAdapter>>>,
}

impl AdapterRegistry {
    /// Create a new registry for the given machine name.
    pub fn new(machine_name: String) -> Self {
        Self {
            machine_name,
            adapters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register an adapter under the given endpoint name.
    pub async fn register(&self, name: String, adapter: Box<dyn McpAdapter>, transport: String) {
        debug!(endpoint = %name, "Registering adapter");
        self.adapters.write().await.insert(
            name,
            RegisteredAdapter {
                adapter,
                transport,
                last_activity: None,
                stderr_lines: Arc::new(RwLock::new(Vec::new())),
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

    /// Build merged tool catalog from all healthy adapters, with prefixed names.
    pub async fn merged_catalog(&self) -> Vec<ToolInfo> {
        let adapters = self.adapters.read().await;
        let mut catalog = Vec::new();

        for (endpoint_name, entry) in adapters.iter() {
            if !matches!(entry.adapter.health(), HealthStatus::Healthy) {
                debug!(endpoint = %endpoint_name, "Skipping unhealthy adapter");
                continue;
            }

            match entry.adapter.list_tools().await {
                Ok(tools) => {
                    for tool in tools {
                        let prefixed_name =
                            prefix::encode_tool_name(&self.machine_name, endpoint_name, &tool.name);
                        catalog.push(ToolInfo {
                            name: prefixed_name,
                            description: tool.description,
                            input_schema: tool.input_schema,
                        });
                    }
                }
                Err(e) => {
                    warn!(endpoint = %endpoint_name, error = %e, "Failed to list tools");
                }
            }
        }

        catalog
    }

    /// Route a prefixed tool call to the correct adapter.
    pub async fn route_tool_call(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, AdapterError> {
        let (_machine, endpoint, tool) = prefix::decode_tool_name(prefixed_name)
            .map_err(|e| AdapterError::ProtocolError(format!("invalid tool name prefix: {}", e)))?;

        let adapters = self.adapters.read().await;
        let entry = adapters.get(&endpoint).ok_or_else(|| {
            AdapterError::ProtocolError(format!("no adapter found for endpoint '{}'", endpoint))
        })?;

        if !matches!(entry.adapter.health(), HealthStatus::Healthy) {
            return Err(AdapterError::ProtocolError(format!(
                "endpoint '{}' is not healthy",
                endpoint
            )));
        }

        entry.adapter.call_tool(&tool, arguments).await
    }

    /// Get the machine name.
    #[allow(dead_code)] // Accessor kept for future use
    pub fn machine_name(&self) -> &str {
        &self.machine_name
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
    }

    impl MockAdapter {
        fn healthy(tools: Vec<ToolInfo>) -> Self {
            Self {
                health: HealthStatus::Healthy,
                tools,
            }
        }

        fn unhealthy() -> Self {
            Self {
                health: HealthStatus::Unhealthy("test".into()),
                tools: vec![],
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
        async fn shutdown(&mut self) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    fn make_tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: Some(format!("{} tool", name)),
            input_schema: json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn test_merged_catalog_with_multiple_adapters() {
        let registry = AdapterRegistry::new("laptop".into());
        registry
            .register(
                "ep1".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("read")])),
                "stdio".into(),
            )
            .await;
        registry
            .register(
                "ep2".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("write")])),
                "stdio".into(),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 2);

        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"laptop__ep1__read"));
        assert!(names.contains(&"laptop__ep2__write"));
    }

    #[tokio::test]
    async fn test_unhealthy_excluded_from_catalog() {
        let registry = AdapterRegistry::new("m".into());
        registry
            .register(
                "good".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("tool")])),
                "stdio".into(),
            )
            .await;
        registry
            .register(
                "bad".into(),
                Box::new(MockAdapter::unhealthy()),
                "stdio".into(),
            )
            .await;

        let catalog = registry.merged_catalog().await;
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "m__good__tool");
    }

    #[tokio::test]
    async fn test_route_tool_call() {
        let registry = AdapterRegistry::new("m".into());
        registry
            .register(
                "ep".into(),
                Box::new(MockAdapter::healthy(vec![make_tool("echo")])),
                "stdio".into(),
            )
            .await;

        let result = registry
            .route_tool_call("m__ep__echo", json!({"msg": "hi"}))
            .await
            .unwrap();
        assert_eq!(result["called"], "echo");
        assert_eq!(result["args"]["msg"], "hi");
    }

    #[tokio::test]
    async fn test_route_invalid_prefix() {
        let registry = AdapterRegistry::new("m".into());
        let result = registry.route_tool_call("bad_name", json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_route_missing_endpoint() {
        let registry = AdapterRegistry::new("m".into());
        let result = registry
            .route_tool_call("m__nonexistent__tool", json!({}))
            .await;
        assert!(result.is_err());
    }
}
