pub mod adapter;
pub mod advertise;
pub mod config;
pub mod container_runtime;
pub mod container_stats;
pub mod events;
pub mod js_sandbox;
pub mod jsonrpc;
pub mod listen_ips;
pub mod local_network;
pub mod management;
pub mod management_listener;
pub mod oauth;
pub mod observability;
pub mod prefix;
pub mod profile_registry;
pub mod protocol;
pub mod registry;
pub mod resource_uri;
pub mod server;
pub mod shell_env;
pub mod token_manager;
pub mod token_security;
pub mod tool_call_rewrite;
pub mod toon_convert;
pub mod watcher;

use adapter::oauth::OAuthAdapterInner;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-endpoint shared OAuth adapter inner states, keyed by endpoint name.
/// Used by the callback handler to apply tokens to the correct adapter.
pub type OAuthAdapterInners = Arc<RwLock<HashMap<String, Arc<OAuthAdapterInner>>>>;

/// Test-only support for stabilising `tracing` callsite-interest caching.
/// Declared in both crate roots (`lib.rs` and `main.rs`) because the binary
/// re-declares the same module files, so `crate::test_tracing` must resolve in
/// each crate.
#[cfg(test)]
pub(crate) mod test_tracing;
