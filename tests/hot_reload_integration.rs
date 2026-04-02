//! Integration test: Config hot reload.
//!
//! Creates a temp config file, starts relay with it,
//! modifies config to add/remove endpoints, verifies changes are applied.

use endara_relay::adapter::stdio::{StdioAdapter, StdioConfig};
use endara_relay::adapter::McpAdapter;
use endara_relay::config;
use endara_relay::registry::AdapterRegistry;
use endara_relay::token_manager::TokenManager;
use endara_relay::watcher;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

fn echo_script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("echo_mcp_server.sh")
}

fn multi_tool_bin() -> String {
    env!("CARGO_BIN_EXE_fixture-multi-tool-server").to_string()
}

#[tokio::test]
async fn test_config_diff_add_endpoint() {
    let registry = AdapterRegistry::new();

    // Start with echo endpoint
    let echo_config = StdioConfig {
        command: "bash".to_string(),
        args: vec![echo_script_path().to_string_lossy().to_string()],
        env: HashMap::new(),
    };
    let mut echo_adapter = StdioAdapter::new(echo_config);
    echo_adapter.initialize().await.expect("echo init failed");
    registry
        .register(
            "echo-ep".into(),
            Box::new(echo_adapter),
            "stdio".into(),
            None,
            Some("echo_ep".into()),
        )
        .await;

    // Verify initial state: 1 endpoint
    let catalog = registry.merged_catalog().await;
    assert_eq!(catalog.len(), 1, "should start with 1 tool");

    // Simulate adding a new endpoint via config diff
    let old_config = config::Config {
        relay: config::RelayConfig {
            machine_name: "testmachine".to_string(),
            local_js_execution: None,
            token_dir: None,
        },
        endpoints: vec![config::EndpointConfig {
            name: "echo-ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: config::Transport::Stdio,
            command: Some("bash".to_string()),
            args: Some(vec![echo_script_path().to_string_lossy().to_string()]),
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
        }],
    };

    let new_config = config::Config {
        relay: config::RelayConfig {
            machine_name: "testmachine".to_string(),
            local_js_execution: None,
            token_dir: None,
        },
        endpoints: vec![
            config::EndpointConfig {
                name: "echo-ep".to_string(),
                description: None,
                tool_prefix: None,
                transport: config::Transport::Stdio,
                command: Some("bash".to_string()),
                args: Some(vec![echo_script_path().to_string_lossy().to_string()]),
                url: None,
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
            },
            config::EndpointConfig {
                name: "multi-ep".to_string(),
                description: None,
                tool_prefix: None,
                transport: config::Transport::Stdio,
                command: Some(multi_tool_bin()),
                args: Some(vec![]),
                url: None,
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
            },
        ],
    };

    let diff = config::diff_configs(&old_config, &new_config);
    assert_eq!(diff.added.len(), 1);
    assert_eq!(diff.added[0].name, "multi-ep");
    assert_eq!(diff.unchanged.len(), 1);

    // Apply the diff
    let tmp = tempfile::tempdir().unwrap();
    let token_manager = Arc::new(TokenManager::new(tmp.path().to_path_buf()));
    let oauth_token_notifiers: Arc<
        RwLock<HashMap<String, tokio::sync::watch::Sender<Option<String>>>>,
    > = Arc::new(RwLock::new(HashMap::new()));
    watcher::apply_diff(&diff, &registry, &token_manager, &oauth_token_notifiers).await;

    // Wait for background initialization to complete
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify the new endpoint was added
    let catalog = registry.merged_catalog().await;
    assert!(
        catalog.len() > 1,
        "should have more than 1 tool after adding multi-ep"
    );
}

#[tokio::test]
async fn test_config_diff_remove_endpoint() {
    let registry = AdapterRegistry::new();

    // Start with 2 endpoints
    let echo_config = StdioConfig {
        command: "bash".to_string(),
        args: vec![echo_script_path().to_string_lossy().to_string()],
        env: HashMap::new(),
    };
    let mut echo_adapter = StdioAdapter::new(echo_config);
    echo_adapter.initialize().await.expect("echo init failed");
    registry
        .register(
            "echo-ep".into(),
            Box::new(echo_adapter),
            "stdio".into(),
            None,
            Some("echo_ep".into()),
        )
        .await;

    let multi_config = StdioConfig {
        command: multi_tool_bin(),
        args: vec![],
        env: HashMap::new(),
    };
    let mut multi_adapter = StdioAdapter::new(multi_config);
    multi_adapter.initialize().await.expect("multi init failed");
    registry
        .register(
            "multi-ep".into(),
            Box::new(multi_adapter),
            "stdio".into(),
            None,
            Some("multi_ep".into()),
        )
        .await;

    let catalog = registry.merged_catalog().await;
    assert!(catalog.len() > 1, "should start with more than 1 tool");

    // Simulate removing multi-ep
    let old_config = config::Config {
        relay: config::RelayConfig {
            machine_name: "testmachine".to_string(),
            local_js_execution: None,
            token_dir: None,
        },
        endpoints: vec![
            config::EndpointConfig {
                name: "echo-ep".to_string(),
                description: None,
                tool_prefix: None,
                transport: config::Transport::Stdio,
                command: Some("bash".to_string()),
                args: Some(vec![echo_script_path().to_string_lossy().to_string()]),
                url: None,
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
            },
            config::EndpointConfig {
                name: "multi-ep".to_string(),
                description: None,
                tool_prefix: None,
                transport: config::Transport::Stdio,
                command: Some(multi_tool_bin()),
                args: Some(vec![]),
                url: None,
                env: None,
                headers: None,
                disabled: false,
                disabled_tools: Vec::new(),
                oauth_server_url: None,
                client_id: None,
                client_secret: None,
                scopes: None,
            },
        ],
    };

    let new_config = config::Config {
        relay: config::RelayConfig {
            machine_name: "testmachine".to_string(),
            local_js_execution: None,
            token_dir: None,
        },
        endpoints: vec![config::EndpointConfig {
            name: "echo-ep".to_string(),
            description: None,
            tool_prefix: None,
            transport: config::Transport::Stdio,
            command: Some("bash".to_string()),
            args: Some(vec![echo_script_path().to_string_lossy().to_string()]),
            url: None,
            env: None,
            headers: None,
            disabled: false,
            disabled_tools: Vec::new(),
            oauth_server_url: None,
            client_id: None,
            client_secret: None,
            scopes: None,
        }],
    };

    let diff = config::diff_configs(&old_config, &new_config);
    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.removed[0], "multi-ep");

    let tmp2 = tempfile::tempdir().unwrap();
    let token_manager2 = Arc::new(TokenManager::new(tmp2.path().to_path_buf()));
    let oauth_token_notifiers2: Arc<
        RwLock<HashMap<String, tokio::sync::watch::Sender<Option<String>>>>,
    > = Arc::new(RwLock::new(HashMap::new()));
    watcher::apply_diff(&diff, &registry, &token_manager2, &oauth_token_notifiers2).await;

    // Verify the endpoint was removed
    let catalog = registry.merged_catalog().await;
    assert_eq!(
        catalog.len(),
        1,
        "should have 1 tool after removing multi-ep"
    );
}
