pub mod adapter;
pub mod config;
pub mod js_sandbox;
pub mod jsonrpc;
pub mod management;
pub mod oauth;
pub mod prefix;
pub mod registry;
pub mod server;
pub mod shell_env;
pub mod token_manager;
pub mod token_security;
pub mod watcher;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-endpoint OAuth token notifiers: endpoint_name → watch::Sender.
pub type OAuthTokenNotifiers =
    Arc<RwLock<HashMap<String, tokio::sync::watch::Sender<Option<String>>>>>;
