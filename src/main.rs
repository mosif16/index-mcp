mod config;
mod database;
mod ingest;
mod lookup;
mod rpc;
mod server;
mod tool;
mod tools;

use anyhow::Result;
use clap::Parser;

use crate::config::Args;
use crate::server::Server;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = config::Config::from_args(&args)?;
    let server = Server::new(config);
    server.run().await
}
