mod bundle;
mod environment;
mod git_timeline;
mod graph;
mod graph_neighbors;
mod index_status;
mod ingest;
mod remote_proxy;
mod search;
mod service;
mod watcher;

use anyhow::Result;
use clap::Parser;
use rmcp::{transport::stdio, ServiceExt};
use std::{path::PathBuf, time::Duration};
use tracing_subscriber::{fmt, EnvFilter};

use crate::index_status::DEFAULT_DB_FILENAME;
use crate::watcher::{start_ingest_watcher, WatcherOptions};

/// Command-line arguments for the Rust MCP server.
#[derive(Debug, Parser)]
#[command(
    name = "index-mcp-server",
    version,
    about = "Rust implementation of the index MCP server"
)]
struct Cli {
    /// Optional working directory override.
    #[arg(long)]
    cwd: Option<String>,

    /// Log level filter (e.g. info, debug, trace).
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Enable file watcher mode.
    #[arg(long)]
    watch: bool,

    /// Override the root directory for the file watcher.
    #[arg(long = "watch-root")]
    watch_root: Option<String>,

    /// Debounce interval in milliseconds for watcher-triggered ingests.
    #[arg(long = "watch-debounce")]
    watch_debounce: Option<u64>,

    /// Disable the initial full ingest when the watcher starts.
    #[arg(long = "watch-no-initial")]
    watch_no_initial: bool,

    /// Reduce watcher logging noise.
    #[arg(long = "watch-quiet")]
    watch_quiet: bool,

    /// Database name to use for watcher ingests.
    #[arg(long = "watch-database")]
    watch_database: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let env_filter = EnvFilter::try_new(cli.log_level.clone())
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    if let Some(path) = cli.cwd.as_ref() {
        std::env::set_current_dir(path)?;
    }

    tracing::info!("Starting Rust MCP server");

    let mut watcher_handle = None;
    if cli.watch {
        let root = cli
            .watch_root
            .clone()
            .or_else(|| cli.cwd.clone())
            .unwrap_or_else(|| ".".to_string());
        let debounce_ms = cli.watch_debounce.unwrap_or(500).max(50);
        let options = WatcherOptions {
            root: PathBuf::from(root),
            database_name: cli
                .watch_database
                .clone()
                .unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string()),
            debounce: Duration::from_millis(debounce_ms),
            run_initial: !cli.watch_no_initial,
            quiet: cli.watch_quiet,
        };

        match start_ingest_watcher(options).await {
            Ok(handle) => {
                watcher_handle = Some(handle);
            }
            Err(error) => {
                tracing::error!(?error, "Failed to start ingest watcher");
            }
        }
    }

    let service = service::IndexMcpService::new().await?;
    let server = service.serve(stdio()).await.map_err(anyhow::Error::from)?;

    // Wait until the client disconnects or the server shuts down.
    server.waiting().await.map_err(anyhow::Error::from)?;

    if let Some(handle) = watcher_handle {
        handle.stop().await;
    }

    Ok(())
}
