#[path = "../bundle.rs"]
mod bundle;
#[path = "../git_timeline.rs"]
mod git_timeline;
#[path = "../graph.rs"]
mod graph;
#[path = "../graph_neighbors.rs"]
mod graph_neighbors;
#[path = "../index_status.rs"]
mod index_status;
#[path = "../ingest.rs"]
mod ingest;
#[path = "../search.rs"]
mod search;

use bundle::{context_bundle, ContextBundleParams, ContextBundleResponse};
use git_timeline::{repository_timeline, RepositoryTimelineParams};
use graph_neighbors::{
    graph_neighbors, GraphNeighborDirection, GraphNeighborsParams, GraphNodeSelector,
};
use index_status::{get_index_status, IndexStatusParams};
use ingest::{ingest_codebase, IngestParams, IngestResponse};
use search::{
    semantic_search, summarize_semantic_search, SemanticSearchParams, SemanticSearchResponse,
};
use std::path::{Path, PathBuf};

#[tokio::main]
async fn main() {
    let root = std::env::var("INDEX_MCP_DEBUG_ROOT").unwrap_or_else(|_| ".".to_string());
    let query = std::env::var("INDEX_MCP_DEBUG_QUERY").unwrap_or_else(|_| "mcp".to_string());
    let verbose = std::env::var("INDEX_MCP_DEBUG_VERBOSE")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true"))
        .unwrap_or(false);

    println!("=== ingest_codebase (root={root}) ===");
    let ingest_response = match run_ingest(&root).await {
        Ok(response) => {
            summarize_ingest(&response, verbose);
            Some(response)
        }
        Err(error) => {
            eprintln!("ingest_codebase failed: {error:?}");
            None
        }
    };

    println!("\n=== semantic_search query='{query}' ===");
    let search_response = match run_semantic_search(&root, &query).await {
        Ok(response) => {
            summarize_search(&response, verbose);
            Some(response)
        }
        Err(error) => {
            eprintln!("semantic_search failed: {error:?}");
            None
        }
    };

    println!("\n=== code_lookup (search mode approximation) ===");
    if let Some(response) = search_response.as_ref() {
        summarize_code_lookup_search(response);
    } else {
        println!("code_lookup (search) skipped due to missing semantic_search result");
    }

    let candidate_files = gather_candidate_files(&root, &search_response);
    let bundle_path = candidate_files
        .iter()
        .find(|path| Path::new(&root).join(path).exists())
        .cloned()
        .unwrap_or_else(|| "README.md".to_string());

    println!("\n=== context_bundle file='{bundle_path}' ===");
    let bundle_response = match run_context_bundle(&root, &bundle_path).await {
        Ok(response) => {
            summarize_context_bundle(&response, verbose);
            Some(response)
        }
        Err(error) => {
            eprintln!("context_bundle failed: {error:?}");
            None
        }
    };

    println!("\n=== code_lookup (bundle mode approximation) ===");
    if let Some(response) = bundle_response.as_ref() {
        summarize_code_lookup_bundle(response, &bundle_path);
    } else {
        println!("code_lookup (bundle) skipped due to missing context_bundle result");
    }

    println!("\n=== graph_neighbors ===");
    if let Some(response) = bundle_response.as_ref() {
        if let Some(definition) = response
            .focus_definition
            .as_ref()
            .or_else(|| response.definitions.first())
        {
            if let Err(error) = run_graph_neighbors(&root, &definition.id, &definition.name).await {
                eprintln!("graph_neighbors failed: {error:?}");
            }
        } else {
            println!("graph_neighbors skipped: no definition found in bundle response");
        }
    } else {
        println!("graph_neighbors skipped due to missing context_bundle result");
    }

    println!("\n=== index_status ===");
    match run_index_status(&root).await {
        Ok(response) => summarize_index_status(&response, verbose),
        Err(error) => eprintln!("index_status failed: {error:?}"),
    }

    println!("\n=== repository_timeline ===");
    match run_repository_timeline(&root).await {
        Ok(response) => summarize_repository_timeline(&response, verbose),
        Err(error) => eprintln!("repository_timeline failed: {error:?}"),
    }

    println!("\n=== debug run complete ===");
    if ingest_response.is_none() {
        println!("Reminder: ingest_codebase failed; downstream results may be stale or empty.");
    }
}

async fn run_ingest(root: &str) -> Result<IngestResponse, ingest::IngestError> {
    let params = IngestParams {
        root: Some(root.to_string()),
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

    ingest_codebase(params).await
}

async fn run_semantic_search(
    root: &str,
    query: &str,
) -> Result<SemanticSearchResponse, search::SemanticSearchError> {
    let params = SemanticSearchParams {
        root: Some(root.to_string()),
        query: query.to_string(),
        database_name: None,
        limit: Some(5),
        model: None,
    };

    semantic_search(params).await
}

async fn run_context_bundle(
    root: &str,
    file: &str,
) -> Result<ContextBundleResponse, bundle::ContextBundleError> {
    let params = ContextBundleParams {
        root: Some(root.to_string()),
        database_name: None,
        file: file.to_string(),
        symbol: None,
        max_snippets: Some(5),
        max_neighbors: Some(10),
        budget_tokens: Some(3_000),
    };

    context_bundle(params).await
}

async fn run_graph_neighbors(
    root: &str,
    node_id: &str,
    node_name: &str,
) -> Result<(), graph_neighbors::GraphNeighborsError> {
    let params = GraphNeighborsParams {
        root: Some(root.to_string()),
        database_name: None,
        node: GraphNodeSelector {
            id: Some(node_id.to_string()),
            path: None,
            kind: None,
            name: node_name.to_string(),
        },
        direction: Some(GraphNeighborDirection::Both),
        limit: Some(10),
    };

    let response = graph_neighbors(params).await?;
    println!(
        "graph_neighbors: found {} neighbors for node {}",
        response.neighbors.len(),
        response.node.name
    );

    Ok(())
}

async fn run_index_status(
    root: &str,
) -> Result<index_status::IndexStatusResponse, index_status::IndexStatusError> {
    let params = IndexStatusParams {
        root: Some(root.to_string()),
        database_name: None,
        history_limit: Some(5),
    };

    get_index_status(params).await
}

async fn run_repository_timeline(
    root: &str,
) -> Result<git_timeline::RepositoryTimelineResponse, git_timeline::RepositoryTimelineError> {
    let params = RepositoryTimelineParams {
        root: Some(root.to_string()),
        branch: None,
        limit: Some(5),
        since: None,
        include_merges: Some(true),
        include_file_stats: Some(true),
        include_diffs: Some(false),
        paths: None,
        diff_pattern: None,
    };

    repository_timeline(params).await
}

fn summarize_ingest(response: &IngestResponse, verbose: bool) {
    println!(
        "ingest_codebase: {files} file(s), {chunks} embedded chunk(s), db={path}",
        files = response.ingested_file_count,
        chunks = response.embedded_chunk_count,
        path = response.database_path
    );

    if verbose {
        dump_json("ingest_codebase response", response);
    }
}

fn summarize_search(response: &SemanticSearchResponse, verbose: bool) {
    println!("{}", summarize_semantic_search(response));

    if let Some(first) = response.results.first() {
        println!(
            "top match: {path} (score={score:.4})",
            path = first.path,
            score = first.normalized_score
        );
    }

    if verbose {
        dump_json("semantic_search response", response);
    }
}

fn summarize_code_lookup_search(response: &SemanticSearchResponse) {
    println!(
        "code_lookup (search): mirrored {count} semantic_search result(s)",
        count = response.results.len()
    );
}

fn summarize_context_bundle(response: &ContextBundleResponse, verbose: bool) {
    println!(
        "context_bundle: {definitions} definition(s), {snippets} snippet(s), {neighbors} related symbol(s)",
        definitions = response.definitions.len(),
        snippets = response.snippets.len(),
        neighbors = response.related.len()
    );

    if let Some(focus) = response
        .focus_definition
        .as_ref()
        .or_else(|| response.definitions.first())
    {
        println!("focus symbol: {} ({})", focus.name, focus.kind);
    }

    if verbose {
        dump_json("context_bundle response", response);
    }
}

fn summarize_code_lookup_bundle(response: &ContextBundleResponse, file: &str) {
    println!(
        "code_lookup (bundle): reused bundle for {file}, {definitions} definition(s)",
        file = file,
        definitions = response.definitions.len()
    );
}

fn summarize_index_status(response: &index_status::IndexStatusResponse, verbose: bool) {
    println!(
        "index_status: db_exists={}, total_files={}, total_chunks={}, is_stale={}",
        response.database_exists, response.total_files, response.total_chunks, response.is_stale
    );

    if verbose {
        dump_json("index_status response", response);
    }
}

fn summarize_repository_timeline(
    response: &git_timeline::RepositoryTimelineResponse,
    verbose: bool,
) {
    println!(
        "repository_timeline: {commits} commit(s), merge_commits={merges}, total_insertions={insertions}, total_deletions={deletions}",
        commits = response.total_commits,
        merges = response.merge_commits,
        insertions = response.total_insertions,
        deletions = response.total_deletions
    );

    if let Some(first) = response.entries.first() {
        println!(
            "latest commit: {sha} {subject}",
            sha = first.sha,
            subject = first.subject
        );
    }

    if verbose {
        dump_json("repository_timeline response", response);
    }
}

fn dump_json<T: serde::Serialize>(label: &str, value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(output) => println!("{label} =>\n{output}"),
        Err(error) => eprintln!("failed to render {label} as JSON: {error}"),
    }
}

fn gather_candidate_files(
    root: &str,
    search_response: &Option<SemanticSearchResponse>,
) -> Vec<String> {
    let mut candidates = Vec::new();

    if let Ok(explicit) = std::env::var("INDEX_MCP_DEBUG_FILE") {
        candidates.push(explicit);
    }

    if let Some(response) = search_response {
        for entry in &response.results {
            candidates.push(entry.path.clone());
        }
    }

    let default_candidates = vec!["docs/rust-migration.md", "README.md"];
    for candidate in default_candidates {
        let path = Path::new(root).join(candidate);
        if path.exists() {
            candidates.push(candidate.to_string());
        }
    }

    candidates
        .into_iter()
        .map(|candidate| normalize_path(root, candidate))
        .collect()
}

fn normalize_path(root: &str, candidate: String) -> String {
    let candidate_path = PathBuf::from(&candidate);
    if candidate_path.is_absolute() {
        candidate
    } else {
        let normalized = Path::new(root).join(&candidate_path);
        normalized
            .strip_prefix(Path::new(root))
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| candidate)
    }
}
