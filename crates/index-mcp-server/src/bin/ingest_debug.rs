#[path = "../ingest.rs"]
mod ingest;
#[path = "../graph.rs"]
mod graph;
#[path = "../index_status.rs"]
mod index_status;

use ingest::{ingest_codebase, IngestParams};

#[tokio::main]
async fn main() {
    let params = IngestParams {
        root: Some(".".to_string()),
        include: None,
        exclude: None,
        database_name: None,
        max_file_size_bytes: None,
        store_file_content: None,
        paths: None,
        auto_evict: Some(false),
        max_database_size_bytes: None,
        embedding: None,
    };

    match ingest_codebase(params).await {
        Ok(result) => println!("SUCCESS: {} file(s)", result.ingested_file_count),
        Err(err) => eprintln!("ERROR: {err:?}"),
    }
}
