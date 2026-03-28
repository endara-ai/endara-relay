mod config;
mod js_sandbox;
mod management;
mod watcher;

mod adapter;
mod jsonrpc;
mod prefix;
mod registry;
mod server;

use adapter::http::{HttpAdapter, HttpConfig};
use adapter::sse::{SseAdapter, SseConfig};
use adapter::stdio::{StdioAdapter, StdioConfig};
use adapter::McpAdapter;
use clap::{Parser, Subcommand};
use js_sandbox::MetaToolHandler;
use registry::AdapterRegistry;
use server::{build_router, start_server, AppState};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
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
        /// Path to TOML configuration file
        #[arg(long, default_value = "~/.endara/config.toml")]
        config: PathBuf,

        /// Port to listen on
        #[arg(long, default_value = "9400")]
        port: u16,

        /// Log output format
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        log_format: String,
    },
}

fn init_tracing(log_format: &str) {
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    // File logging to ~/.endara/logs/ with daily rotation
    let log_dir = dirs::home_dir()
        .map(|h| h.join(".endara").join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/endara-logs"));

    let file_appender = tracing_appender::rolling::daily(&log_dir, "relay.log");
    let file_layer = fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .boxed();

    match log_format {
        "json" => {
            let stdout_layer = fmt::layer().json().with_ansi(false).boxed();
            tracing_subscriber::registry()
                .with(stdout_layer)
                .with(file_layer)
                .init();
        }
        _ => {
            let stdout_layer = fmt::layer().with_ansi(false).boxed();
            tracing_subscriber::registry()
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
            config: config_path,
            port,
            log_format,
        } => {
            init_tracing(&log_format);
            info!(config = %config_path.display(), "Starting endara-relay");

            let cfg = match config::load_config(&config_path) {
                Ok(cfg) => {
                    info!(
                        machine_name = %cfg.relay.machine_name,
                        endpoints = cfg.endpoints.len(),
                        "Configuration loaded successfully"
                    );
                    cfg
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
                            cfg
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

            // Create adapter registry
            let registry = AdapterRegistry::new(cfg.relay.machine_name.clone());

            // Register and initialize adapters for each endpoint
            for ep in &cfg.endpoints {
                info!(name = %ep.name, transport = %ep.transport, "Configuring endpoint");
                match ep.transport {
                    config::Transport::Stdio => {
                        let stdio_config = StdioConfig {
                            command: ep.command.clone().unwrap_or_default(),
                            args: ep.args.clone().unwrap_or_default(),
                            env: ep.env.clone().unwrap_or_default(),
                        };
                        let mut adapter = StdioAdapter::new(stdio_config);
                        match adapter.initialize().await {
                            Ok(()) => {
                                info!(endpoint = %ep.name, "Adapter initialized");
                                registry
                                    .register(
                                        ep.name.clone(),
                                        Box::new(adapter),
                                        ep.transport.to_string(),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                warn!(endpoint = %ep.name, error = %e, "Failed to initialize adapter, skipping");
                            }
                        }
                    }
                    config::Transport::Sse => {
                        let url = ep.url.clone().unwrap_or_default();
                        let sse_config = SseConfig::new(url);
                        let mut adapter = SseAdapter::new(sse_config);
                        match adapter.initialize().await {
                            Ok(()) => {
                                info!(endpoint = %ep.name, "SSE adapter initialized");
                                registry
                                    .register(
                                        ep.name.clone(),
                                        Box::new(adapter),
                                        ep.transport.to_string(),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                warn!(endpoint = %ep.name, error = %e, "Failed to initialize SSE adapter, skipping");
                            }
                        }
                    }
                    config::Transport::Http => {
                        let url = ep.url.clone().unwrap_or_default();
                        let http_config = HttpConfig::new(url);
                        let mut adapter = HttpAdapter::new(http_config);
                        match adapter.initialize().await {
                            Ok(()) => {
                                info!(endpoint = %ep.name, "HTTP adapter initialized");
                                registry
                                    .register(
                                        ep.name.clone(),
                                        Box::new(adapter),
                                        ep.transport.to_string(),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                warn!(endpoint = %ep.name, error = %e, "Failed to initialize HTTP adapter, skipping");
                            }
                        }
                    }
                }
            }

            // Build and start HTTP server
            let registry = Arc::new(registry);
            let js_execution_mode = Arc::new(AtomicBool::new(
                cfg.relay.local_js_execution.unwrap_or(false),
            ));
            let meta_tool_handler = Arc::new(MetaToolHandler::new(
                registry.clone(),
                Duration::from_secs(30),
            ));
            let state = AppState {
                registry: (*registry).clone(),
                js_execution_mode: js_execution_mode.clone(),
                meta_tool_handler,
            };
            let mgmt_state = management::ManagementState {
                registry: registry.clone(),
                config: Arc::new(tokio::sync::RwLock::new(cfg.clone())),
                start_time: std::time::Instant::now(),
                config_path: Some(config_path.clone()),
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
                    );

                    handle.await.ok();
                }
                Err(e) => {
                    error!(error = %e, "Failed to start HTTP server");
                    std::process::exit(1);
                }
            }
        }
    }
}
