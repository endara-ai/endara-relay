mod config;
mod js_sandbox;
mod management;
mod oauth;
mod token_manager;
mod watcher;

mod adapter;
mod jsonrpc;
mod prefix;
mod registry;
mod server;
mod shell_env;

use adapter::oauth::{OAuthAdapter, OAuthAdapterConfig, OAuthAdapterInner};
use adapter::{FailedAdapter, McpAdapter, StartingAdapter};
use clap::{Parser, Subcommand};
use js_sandbox::MetaToolHandler;
use oauth::{OAuthFlowManager, OAuthSetupManager};
use registry::AdapterRegistry;
use server::{build_router, start_server, AppState};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use token_manager::TokenManager;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Per-endpoint shared OAuth adapter inner states, keyed by endpoint name.
/// Used by the callback handler to apply tokens to the correct adapter.
pub type OAuthAdapterInners = Arc<RwLock<HashMap<String, Arc<OAuthAdapterInner>>>>;
use watcher::ConfigWatcher;

#[derive(Parser)]
#[command(name = "endara-relay", about = "Endara Relay agent")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the relay agent
    Start {
        /// Base data directory for config, logs, and tokens
        #[arg(long, default_value = "~/.endara")]
        data_dir: String,

        /// Path to TOML configuration file (overrides <data-dir>/config.toml)
        #[arg(long)]
        config: Option<PathBuf>,

        /// Port to listen on
        #[arg(long, default_value = "9400")]
        port: u16,

        /// Log output format
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        log_format: String,
    },
}

/// Expand a path string, replacing a leading `~` with the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    }
}

fn init_tracing(log_format: &str, log_dir: &std::path::Path) {
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    // Default: info for all crates, debug for endara_relay.
    // Overridable via RUST_LOG env var.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,endara_relay=debug"));

    let file_appender = tracing_appender::rolling::daily(log_dir, "relay.log");
    let file_layer = fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .boxed();

    match log_format {
        "json" => {
            let stdout_layer = fmt::layer().json().with_ansi(false).boxed();
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .init();
        }
        _ => {
            let stdout_layer = fmt::layer().with_ansi(false).boxed();
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .init();
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            data_dir,
            config,
            port,
            log_format,
        } => {
            let data_dir_path = expand_tilde(&data_dir);
            let log_dir = data_dir_path.join("logs");
            let config_explicit = config.is_some();
            let config_path = config.unwrap_or_else(|| data_dir_path.join("config.toml"));

            init_tracing(&log_format, &log_dir);
            info!(config = %config_path.display(), data_dir = %data_dir_path.display(), "Starting endara-relay");

            // First-run config copy: when using a non-default data dir without an
            // explicit --config, copy the production config so dev instances inherit
            // the same endpoint setup.
            let default_data_dir = dirs::home_dir()
                .map(|h| h.join(".endara"))
                .unwrap_or_default();
            if !config_explicit && data_dir_path != default_data_dir {
                let resolved_config = config::expand_tilde(&config_path);
                if !resolved_config.exists() {
                    let production_config = default_data_dir.join("config.toml");
                    if production_config.exists() {
                        // Ensure the data dir and standard subdirs exist
                        if let Err(e) = std::fs::create_dir_all(&data_dir_path) {
                            warn!(error = %e, path = %data_dir_path.display(), "Failed to create data directory");
                        }
                        for sub in &["logs", "tokens"] {
                            let sub_path = data_dir_path.join(sub);
                            if let Err(e) = std::fs::create_dir_all(&sub_path) {
                                warn!(error = %e, path = %sub_path.display(), "Failed to create subdirectory");
                            }
                        }
                        match std::fs::copy(&production_config, &resolved_config) {
                            Ok(_) => {
                                info!(
                                    src = %production_config.display(),
                                    dst = %resolved_config.display(),
                                    "Copied production config to dev data directory"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    src = %production_config.display(),
                                    dst = %resolved_config.display(),
                                    "Failed to copy production config"
                                );
                            }
                        }
                    }
                }
            }

            let (cfg, validation_warnings) = match config::load_config_graceful(&config_path) {
                Ok((cfg, warnings)) => {
                    info!(
                        machine_name = %cfg.relay.machine_name,
                        endpoints = cfg.endpoints.len(),
                        warnings = warnings.len(),
                        "Configuration loaded successfully"
                    );
                    for w in &warnings {
                        warn!("{}", w);
                    }
                    (cfg, warnings)
                }
                Err(config::ConfigError::IoError(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::NotFound =>
                {
                    info!(
                        path = %config_path.display(),
                        "Config file not found, creating default configuration"
                    );
                    match config::create_default_config_file(&config_path) {
                        Ok(cfg) => {
                            info!(
                                machine_name = %cfg.relay.machine_name,
                                path = %config_path.display(),
                                "Created default configuration"
                            );
                            (cfg, Vec::new())
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to create default configuration");
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to load configuration");
                    std::process::exit(1);
                }
            };

            // Collect endpoint names that have validation warnings
            let warned_names = config::warned_endpoint_names(&validation_warnings);
            // Build a map from endpoint name to its warning message(s)
            let warning_messages: std::collections::HashMap<String, String> = {
                let mut map = std::collections::HashMap::new();
                for w in &validation_warnings {
                    map.entry(w.endpoint_name.clone())
                        .and_modify(|msg: &mut String| {
                            msg.push_str("; ");
                            msg.push_str(&w.message);
                        })
                        .or_insert_with(|| w.message.clone());
                }
                map
            };

            // Initialize OAuth infrastructure
            // ENDARA_TOKEN_DIR env var allows integration tests to isolate token storage.
            let token_dir_path = std::env::var("ENDARA_TOKEN_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| data_dir_path.join("tokens"));
            let token_dir =
                match endara_relay::token_security::ensure_token_dir_secure(&token_dir_path) {
                    Ok(path) => path,
                    Err(e) => {
                        error!(error = %e, "Failed to secure token directory");
                        token_dir_path // Fall back to unsecured path
                    }
                };
            let token_manager = Arc::new(TokenManager::new(token_dir));
            let oauth_flow_manager = Arc::new(OAuthFlowManager::new());
            let oauth_adapter_inners: OAuthAdapterInners = Arc::new(RwLock::new(HashMap::new()));

            // Create adapter registry
            let registry = AdapterRegistry::new();

            // Track duplicate endpoint names: first occurrence wins
            let mut registered_names = std::collections::HashSet::new();

            // Collect endpoints that need background initialization
            let mut deferred_init: Vec<config::EndpointConfig> = Vec::new();

            // Register adapters for each endpoint (non-blocking)
            for ep in &cfg.endpoints {
                // Handle duplicate endpoint names: first wins, rest skipped
                if !ep.name.is_empty() && !registered_names.insert(ep.name.clone()) {
                    let msg = format!("Duplicate endpoint name: '{}'", ep.name);
                    warn!(endpoint = %ep.name, "{}", msg);
                    continue;
                }

                // If this endpoint has validation warnings, register as FailedAdapter
                if warned_names.contains(&ep.name) {
                    let msg = warning_messages.get(&ep.name).cloned().unwrap_or_default();
                    warn!(endpoint = %ep.name, "Registering as failed due to validation error: {}", msg);
                    let adapter: Box<dyn McpAdapter> = Box::new(FailedAdapter::new(msg));
                    registry
                        .register(
                            ep.name.clone(),
                            adapter,
                            ep.transport.to_string(),
                            ep.description.clone(),
                            ep.resolved_tool_prefix(),
                        )
                        .await;
                    continue;
                }

                info!(name = %ep.name, transport = %ep.transport, "Configuring endpoint");

                // OAuth endpoints initialize inline (always fast)
                if ep.transport == config::Transport::Oauth {
                    let base = ep.oauth_server_url.as_deref().unwrap_or_default();
                    let oauth_config = OAuthAdapterConfig {
                        endpoint_name: ep.name.clone(),
                        url: ep.url.clone().unwrap_or_default(),
                        token_endpoint_url: ep
                            .token_endpoint
                            .clone()
                            .unwrap_or_else(|| format!("{}/token", base)),
                        client_id: ep.client_id.clone().unwrap_or_default(),
                        client_secret: ep.client_secret.clone(),
                        heartbeat_interval_secs: 30,
                        probe_timeout_secs: 10,
                        probe_failure_threshold: 3,
                    };

                    let mut adapter = OAuthAdapter::new(oauth_config, token_manager.clone());
                    let shared_inner = adapter.shared_inner();
                    oauth_adapter_inners
                        .write()
                        .await
                        .insert(ep.name.clone(), shared_inner);

                    adapter.initialize().await.ok();
                    info!(endpoint = %ep.name, "OAuth adapter initialized");
                    registry
                        .register(
                            ep.name.clone(),
                            Box::new(adapter),
                            ep.transport.to_string(),
                            ep.description.clone(),
                            ep.resolved_tool_prefix(),
                        )
                        .await;
                    continue;
                }

                // Register with Starting status immediately; initialize in background later
                registry
                    .register(
                        ep.name.clone(),
                        Box::new(StartingAdapter),
                        ep.transport.to_string(),
                        ep.description.clone(),
                        ep.resolved_tool_prefix(),
                    )
                    .await;
                deferred_init.push(ep.clone());
            }

            // Apply disabled state from config
            for ep in &cfg.endpoints {
                if ep.disabled {
                    let mut entries = registry.entries().write().await;
                    if let Some(entry) = entries.get_mut(&ep.name) {
                        entry.disabled = true;
                    }
                }
                if !ep.disabled_tools.is_empty() {
                    let mut entries = registry.entries().write().await;
                    if let Some(entry) = entries.get_mut(&ep.name) {
                        entry.disabled_tools = ep.disabled_tools.iter().cloned().collect();
                    }
                }
            }

            // Build and start HTTP server
            let registry = Arc::new(registry);

            // Spawn background initialization for deferred endpoints
            for ep in deferred_init {
                let reg = registry.clone();
                let tm = token_manager.clone();
                let oai = oauth_adapter_inners.clone();
                tokio::spawn(async move {
                    let adapter = watcher::create_adapter(&ep, &tm, &oai).await;
                    let mut entries = reg.entries().write().await;
                    if let Some(entry) = entries.get_mut(ep.name.as_str()) {
                        entry.adapter = adapter;
                        if entry.disabled {
                            let _ = entry.adapter.shutdown().await;
                        }
                    }
                    drop(entries);
                    reg.invalidate_catalog_cache().await;
                    info!(endpoint = %ep.name, "Adapter initialized");
                });
            }
            let js_execution_mode = Arc::new(AtomicBool::new(
                cfg.relay.local_js_execution.unwrap_or(false),
            ));
            let meta_tool_handler = Arc::new(MetaToolHandler::new(
                registry.clone(),
                Duration::from_secs(30),
            ));
            let setup_manager = Arc::new(OAuthSetupManager::new());
            let state = AppState {
                registry: (*registry).clone(),
                js_execution_mode: js_execution_mode.clone(),
                meta_tool_handler,
                oauth_flow_manager: Some(oauth_flow_manager.clone()),
                token_manager: Some(token_manager.clone()),
                oauth_adapter_inners: Some(oauth_adapter_inners.clone()),
                setup_manager: Some(setup_manager.clone()),
            };
            let mgmt_state = management::ManagementState {
                registry: registry.clone(),
                config: Arc::new(tokio::sync::RwLock::new(cfg.clone())),
                start_time: std::time::Instant::now(),
                config_path: Some(config_path.clone()),
                oauth_flow_manager: Some(oauth_flow_manager.clone()),
                relay_port: port,
                oauth_adapter_inners: Some(oauth_adapter_inners.clone()),
                token_manager: Some(token_manager.clone()),
                setup_manager: Some(setup_manager.clone()),
            };
            let router = build_router(state).merge(management::management_routes(mgmt_state));
            let addr: SocketAddr = ([0, 0, 0, 0], port).into();

            match start_server(router, addr).await {
                Ok((bound_addr, handle)) => {
                    info!(addr = %bound_addr, "MCP server running");

                    // Spawn config file watcher for hot-reload
                    let _watcher_handle = ConfigWatcher::start(
                        config_path.clone(),
                        registry.clone(),
                        cfg.relay.machine_name.clone(),
                        js_execution_mode.clone(),
                        token_manager.clone(),
                        oauth_flow_manager.clone(),
                        oauth_adapter_inners.clone(),
                    );

                    handle.await.ok();

                    // Shut down all adapters gracefully (kills STDIO child processes, etc.)
                    info!("Shutting down all adapters");
                    let mut entries = registry.entries().write().await;
                    for (name, entry) in entries.iter_mut() {
                        info!(endpoint = %name, "Shutting down adapter");
                        if let Err(e) = entry.adapter.shutdown().await {
                            warn!(endpoint = %name, error = %e, "Error shutting down adapter");
                        }
                    }
                    info!("All adapters shut down, exiting");
                }
                Err(e) => {
                    error!(error = %e, "Failed to start HTTP server");
                    std::process::exit(1);
                }
            }
        }
    }
}
