use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

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

        /// Log output format
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        log_format: String,
    },
}

fn init_tracing(log_format: &str) {
    match log_format {
        "json" => {
            tracing_subscriber::fmt().json().init();
        }
        _ => {
            tracing_subscriber::fmt().init();
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start { config, log_format } => {
            init_tracing(&log_format);
            info!(config = %config.display(), "Starting endara-relay");
            info!("Configuration path: {}", config.display());
            info!("Relay started successfully, exiting cleanly");
        }
    }
}

