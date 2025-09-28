use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(long)]
    pub working_dir: Option<PathBuf>,

    #[arg(long, value_name = "NAME")]
    pub database: Option<String>,

    #[arg(long)]
    pub log_level: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub working_dir: PathBuf,
    pub default_database_name: String,
    pub log_level: LogLevel,
}

#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }

    pub fn precedence(&self) -> u8 {
        match self {
            LogLevel::Error => 0,
            LogLevel::Warn => 1,
            LogLevel::Info => 2,
            LogLevel::Debug => 3,
            LogLevel::Trace => 4,
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" | "warning" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            other => Err(anyhow::anyhow!("Unknown log level: {other}")),
        }
    }
}

impl Config {
    pub fn from_args(args: &Args) -> Result<Self> {
        let working_dir = args
            .working_dir
            .clone()
            .or_else(|| env::var_os("INDEX_MCP_WORKING_DIR").map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let default_database_name = args
            .database
            .clone()
            .or_else(|| env::var("INDEX_MCP_DATABASE_NAME").ok())
            .unwrap_or_else(|| ".mcp-index.sqlite".to_string());

        let log_level = args
            .log_level
            .clone()
            .or_else(|| env::var("INDEX_MCP_LOG_LEVEL").ok())
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(LogLevel::Info);

        Ok(Config {
            working_dir,
            default_database_name,
            log_level,
        })
    }
}
