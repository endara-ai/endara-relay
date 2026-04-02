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

use adapter::oauth::OAuthAdapterInner;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-endpoint shared OAuth adapter inner states, keyed by endpoint name.
/// Used by the callback handler to apply tokens to the correct adapter.
pub type OAuthAdapterInners = Arc<RwLock<HashMap<String, Arc<OAuthAdapterInner>>>>;
