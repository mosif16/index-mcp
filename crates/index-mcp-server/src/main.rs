mod bundle;
mod git_timeline;
mod graph;
mod index_status;
mod ingest;
mod remote_proxy;
mod search;
mod service;
mod watcher;

use anyhow::Result;
use clap::Parser;
use rmcp::{transport::stdio, ServiceExt};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{
    filter::{Directive, LevelFilter},
    fmt, EnvFilter,
};

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

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn create_file_writer(path: &str) -> Result<(NonBlocking, WorkerGuard)> {
    let path_buf = PathBuf::from(path);

    let file_path = if path_buf.extension().is_some() {
        if let Some(parent) = path_buf.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        path_buf
    } else {
        fs::create_dir_all(&path_buf)?;
        path_buf.join("index-mcp.log")
    };

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    Ok((writer, guard))
}

struct LogWriters {
    console_enabled: bool,
    file_writer: Option<NonBlocking>,
}

impl LogWriters {
    fn new(console_enabled: bool, file_writer: Option<NonBlocking>) -> Self {
        Self {
            console_enabled,
            file_writer,
        }
    }
}

impl<'a> fmt::MakeWriter<'a> for LogWriters {
    type Writer = CombinedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        let console = if self.console_enabled {
            Some(std::io::stderr())
        } else {
            None
        };

        let file = self
            .file_writer
            .as_ref()
            .map(|writer| Box::new(writer.make_writer()) as Box<dyn Write + Send>);

        CombinedWriter { console, file }
    }
}

struct CombinedWriter {
    console: Option<std::io::Stderr>,
    file: Option<Box<dyn Write + Send>>,
}

impl Write for CombinedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(buf)?;
        }
        if let Some(console) = self.console.as_mut() {
            console.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        if let Some(console) = self.console.as_mut() {
            console.flush()?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let start_time = Instant::now();
    let cli = Cli::parse();

    let log_filter = if let Ok(value) = env::var("INDEX_MCP_LOG_LEVEL") {
        value
    } else if let Ok(value) = env::var("RUST_LOG") {
        value
    } else {
        cli.log_level.clone()
    };

    let mut env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(log_filter.as_str());

    let lower_filter = log_filter.to_ascii_lowercase();
    let default_directives = [
        ("rmcp::service", "rmcp::service=info"),
        ("rmcp::handler::server", "rmcp::handler::server=info"),
        ("hf_hub", "hf_hub=warn"),
    ];

    for (needle, directive_str) in default_directives {
        if !lower_filter.contains(needle) {
            if let Ok(directive) = directive_str.parse::<Directive>() {
                env_filter = env_filter.add_directive(directive);
            }
        }
    }

    let log_console = env::var("INDEX_MCP_LOG_CONSOLE")
        .ok()
        .and_then(|value| parse_bool(value.as_str()))
        .unwrap_or(true);

    let log_dir = env::var("INDEX_MCP_LOG_DIR").ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let mut log_guards: Vec<WorkerGuard> = Vec::new();

    let file_writer = if let Some(dir) = log_dir.as_ref() {
        match create_file_writer(dir) {
            Ok((writer, guard)) => {
                log_guards.push(guard);
                Some(writer)
            }
            Err(error) => {
                eprintln!(
                    "[index-mcp] Warning: unable to configure file logging at '{}': {}",
                    dir, error
                );
                None
            }
        }
    } else {
        None
    };

    fmt()
        .with_env_filter(env_filter)
        .with_writer(LogWriters::new(log_console, file_writer))
        .with_ansi(false)
        .init();
    let _log_guards = log_guards;

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
    tracing::info!(
        elapsed_ms = start_time.elapsed().as_millis() as u64,
        "Server initialization finished"
    );
    let server = service.serve(stdio()).await.map_err(anyhow::Error::from)?;

    // Wait until the client disconnects or the server shuts down.
    server.waiting().await.map_err(anyhow::Error::from)?;

    if let Some(handle) = watcher_handle {
        handle.stop().await;
    }

    Ok(())
}
