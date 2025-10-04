#[path = "../bundle.rs"]
mod bundle;
#[path = "../git_timeline.rs"]
mod git_timeline;
#[path = "../graph.rs"]
mod graph;
#[path = "../index_status.rs"]
mod index_status;
#[path = "../ingest.rs"]
mod ingest;
#[path = "../search.rs"]
mod search;

use bundle::{context_bundle, ContextBundleError, ContextBundleParams, ContextBundleResponse};
use clap::{builder::BoolishValueParser, Parser, ValueEnum, ValueHint};
use git_timeline::{
    repository_timeline, repository_timeline_entry_detail, RepositoryTimelineEntryLookupParams,
    RepositoryTimelineEntryLookupResponse, RepositoryTimelineError, RepositoryTimelineParams,
    RepositoryTimelineResponse,
};
use index_status::{get_index_status, IndexStatusError, IndexStatusParams, IndexStatusResponse};
use ingest::{ingest_codebase, warm_up_embedder, IngestError, IngestParams, IngestResponse};
use search::{
    semantic_search, summarize_semantic_search, SemanticSearchError, SemanticSearchParams,
    SemanticSearchResponse, SummaryMode,
};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;
use tracing::warn;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Debug harness that exercises core MCP tools against a workspace SQLite index",
    long_about = None
)]
struct Cli {
    #[arg(
        long,
        env = "INDEX_MCP_DEBUG_ROOT",
        value_hint = ValueHint::DirPath,
        default_value = "."
    )]
    root: PathBuf,

    #[arg(long, env = "INDEX_MCP_DEBUG_DATABASE")]
    database: Option<String>,

    #[arg(long, env = "INDEX_MCP_DEBUG_QUERY", default_value = "mcp")]
    query: String,

    #[arg(long, env = "INDEX_MCP_DEBUG_FILE")]
    file: Option<String>,

    #[arg(long = "section", value_enum)]
    section: Vec<Section>,

    #[arg(long = "skip-section", value_enum)]
    skip_section: Vec<Section>,

    #[arg(long, env = "INDEX_MCP_DEBUG_LIMIT", default_value_t = 5)]
    limit: u32,

    #[arg(long, env = "INDEX_MCP_DEBUG_MAX_SNIPPETS", default_value_t = 5)]
    max_snippets: u32,

    #[arg(long, env = "INDEX_MCP_DEBUG_MAX_NEIGHBORS", default_value_t = 10)]
    max_neighbors: u32,

    #[arg(long, env = "INDEX_MCP_DEBUG_BUDGET_TOKENS", default_value_t = 3_000)]
    budget_tokens: u32,

    #[arg(
        long,
        env = "INDEX_MCP_DEBUG_VERBOSE",
        default_value_t = false,
        value_parser = BoolishValueParser::new()
    )]
    verbose: bool,

    #[arg(
        long,
        env = "INDEX_MCP_DEBUG_LOG_FORMAT",
        value_enum,
        default_value_t = LogFormat::Text
    )]
    log_format: LogFormat,

    #[arg(long, env = "INDEX_MCP_DEBUG_JSON_REPORT")]
    json_report: Option<PathBuf>,

    #[arg(
        long,
        env = "INDEX_MCP_DEBUG_FAIL_FAST",
        default_value_t = false,
        value_parser = BoolishValueParser::new()
    )]
    fail_fast: bool,

    #[arg(
        long,
        env = "INDEX_MCP_DEBUG_INCLUDE_DIFFS",
        default_value_t = false,
        value_parser = BoolishValueParser::new()
    )]
    include_diffs: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, ValueEnum)]
enum Section {
    Ingest,
    SemanticSearch,
    CodeLookupSearch,
    ContextBundle,
    CodeLookupBundle,
    IndexStatus,
    RepositoryTimeline,
}

impl Section {
    fn all() -> Vec<Self> {
        vec![
            Self::Ingest,
            Self::SemanticSearch,
            Self::CodeLookupSearch,
            Self::ContextBundle,
            Self::CodeLookupBundle,
            Self::IndexStatus,
            Self::RepositoryTimeline,
        ]
    }

    fn key(&self) -> &'static str {
        match self {
            Self::Ingest => "ingest_codebase",
            Self::SemanticSearch => "semantic_search",
            Self::CodeLookupSearch => "code_lookup_search",
            Self::ContextBundle => "context_bundle",
            Self::CodeLookupBundle => "code_lookup_bundle",
            Self::IndexStatus => "index_status",
            Self::RepositoryTimeline => "repository_timeline",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

impl Default for LogFormat {
    fn default() -> Self {
        Self::Text
    }
}

struct RunConfig {
    root: PathBuf,
    database: Option<String>,
    query: String,
    bundle_file: Option<String>,
    limit: u32,
    max_snippets: u32,
    max_neighbors: u32,
    budget_tokens: u32,
    verbose: bool,
    log_format: LogFormat,
    json_report: Option<PathBuf>,
    sections: Vec<Section>,
    fail_fast: bool,
    include_diffs: bool,
}

impl RunConfig {
    fn from_cli(cli: Cli) -> Self {
        let Cli {
            root,
            database,
            query,
            file,
            section,
            skip_section,
            limit,
            max_snippets,
            max_neighbors,
            budget_tokens,
            verbose,
            log_format,
            json_report,
            fail_fast,
            include_diffs,
        } = cli;

        let sections = determine_sections(section, skip_section);

        Self {
            root,
            database,
            query,
            bundle_file: file,
            limit,
            max_snippets,
            max_neighbors,
            budget_tokens,
            verbose,
            log_format,
            json_report,
            sections,
            fail_fast,
            include_diffs,
        }
    }
}

fn determine_sections(requested: Vec<Section>, skipped: Vec<Section>) -> Vec<Section> {
    let mut sections = if requested.is_empty() {
        Section::all()
    } else {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        for section in requested {
            if seen.insert(section) {
                ordered.push(section);
            }
        }
        ordered
    };

    for section in skipped {
        sections.retain(|candidate| candidate != &section);
    }

    if sections.is_empty() {
        sections = Section::all();
    }

    sections
}

#[derive(Default)]
struct RunState {
    ingest: Option<IngestResponse>,
    search: Option<SemanticSearchResponse>,
    bundle: Option<ContextBundleResponse>,
    timeline: Option<RepositoryTimelineResponse>,
}

#[derive(Debug, Serialize)]
struct SectionSummary {
    name: &'static str,
    status: &'static str,
    duration_ms: u128,
    message: Option<String>,
}

enum SectionExecution {
    Success { message: Option<String> },
    Skipped { reason: Option<String> },
    Failed { error: String },
}

impl SectionExecution {
    fn status(&self) -> &'static str {
        match self {
            Self::Success { .. } => "success",
            Self::Skipped { .. } => "skipped",
            Self::Failed { .. } => "failed",
        }
    }

    fn message(&self) -> Option<&String> {
        match self {
            Self::Success { message } => message.as_ref(),
            Self::Skipped { reason } => reason.as_ref(),
            Self::Failed { error } => Some(error),
        }
    }

    fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = RunConfig::from_cli(cli);
    tokio::spawn(async {
        match tokio::task::spawn_blocking(|| warm_up_embedder(None)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(?error, "Embedder warm-up failed"),
            Err(join_error) => warn!(?join_error, "Embedder warm-up task cancelled"),
        }
    });
    tokio::task::yield_now().await;
    let exit_code = execute(config).await;
    if exit_code != 0 {
        process::exit(exit_code);
    }
}

async fn execute(config: RunConfig) -> i32 {
    let mut state = RunState::default();
    let mut summaries = Vec::new();
    let mut exit_code = 0;

    for section in &config.sections {
        let start = Instant::now();
        let execution = execute_section(*section, &config, &mut state).await;
        let duration_ms = start.elapsed().as_millis();
        let header = section_header(*section, &config);

        emit_log(
            config.log_format,
            *section,
            &header,
            duration_ms,
            &execution,
        );

        summaries.push(SectionSummary {
            name: section.key(),
            status: execution.status(),
            duration_ms,
            message: execution.message().map(|m| m.to_owned()),
        });

        if execution.is_failure() {
            exit_code = 1;
            if config.fail_fast {
                break;
            }
        }
    }

    if let Some(path) = &config.json_report {
        if let Err(error) = write_json_report(path, &summaries) {
            eprintln!(
                "failed to write JSON report to {}: {}",
                path.display(),
                error
            );
            exit_code = 1;
        }
    }

    exit_code
}

async fn execute_section(
    section: Section,
    config: &RunConfig,
    state: &mut RunState,
) -> SectionExecution {
    match section {
        Section::Ingest => ingest_section(config, state).await,
        Section::SemanticSearch => semantic_search_section(config, state).await,
        Section::CodeLookupSearch => code_lookup_search_section(state),
        Section::ContextBundle => context_bundle_section(config, state).await,
        Section::CodeLookupBundle => code_lookup_bundle_section(state),
        Section::IndexStatus => index_status_section(config).await,
        Section::RepositoryTimeline => repository_timeline_section(config, state).await,
    }
}

fn section_header(section: Section, config: &RunConfig) -> String {
    match section {
        Section::Ingest => format!("ingest_codebase (root={})", config.root.display()),
        Section::SemanticSearch => {
            format!("semantic_search query='{}'", config.query)
        }
        Section::CodeLookupSearch => "code_lookup (search mode approximation)".to_string(),
        Section::ContextBundle => match &config.bundle_file {
            Some(file) => format!("context_bundle file='{file}'"),
            None => "context_bundle".to_string(),
        },
        Section::CodeLookupBundle => "code_lookup (bundle mode approximation)".to_string(),
        Section::IndexStatus => "index_status".to_string(),
        Section::RepositoryTimeline => "repository_timeline".to_string(),
    }
}

fn emit_log(
    format: LogFormat,
    section: Section,
    header: &str,
    duration_ms: u128,
    execution: &SectionExecution,
) {
    match format {
        LogFormat::Text => {
            println!("=== {header} ===");
            match execution {
                SectionExecution::Success { message } => {
                    if let Some(message) = message {
                        println!("{message}");
                    }
                    println!("status: success ({} ms)", duration_ms);
                }
                SectionExecution::Skipped { reason } => {
                    println!("status: skipped ({} ms)", duration_ms);
                    if let Some(reason) = reason {
                        println!("reason: {reason}");
                    }
                }
                SectionExecution::Failed { error } => {
                    println!("status: failed ({} ms)", duration_ms);
                    println!("error: {error}");
                }
            }
            println!();
        }
        LogFormat::Json => {
            let entry = SectionSummary {
                name: section.key(),
                status: execution.status(),
                duration_ms,
                message: execution.message().map(|m| m.to_owned()),
            };
            match serde_json::to_string(&entry) {
                Ok(line) => println!("{line}"),
                Err(error) => eprintln!("failed to serialize log entry: {error}"),
            }
        }
    }
}

fn write_json_report(path: &Path, summaries: &[SectionSummary]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let payload = serde_json::to_string_pretty(summaries).map_err(io::Error::other)?;
    fs::write(path, payload)
}

async fn ingest_section(config: &RunConfig, state: &mut RunState) -> SectionExecution {
    match run_ingest(config).await {
        Ok(response) => {
            let message = summarize_ingest(&response);
            if config.verbose {
                dump_json("ingest_codebase response", &response);
            }
            state.ingest = Some(response);
            SectionExecution::Success {
                message: Some(message),
            }
        }
        Err(error) => SectionExecution::Failed {
            error: error.to_string(),
        },
    }
}

async fn semantic_search_section(config: &RunConfig, state: &mut RunState) -> SectionExecution {
    match run_semantic_search(config).await {
        Ok(response) => {
            let mut lines = vec![summarize_semantic_search(&response)];
            if let Some(first) = response.results.first() {
                lines.push(format!(
                    "top match: {} (score={:.4})",
                    first.path, first.normalized_score
                ));
            }
            lines.extend(suggestion_lines(&response));
            if config.verbose {
                dump_json("semantic_search response", &response);
            }
            state.search = Some(response);
            SectionExecution::Success {
                message: Some(lines.join("\n")),
            }
        }
        Err(error) => SectionExecution::Failed {
            error: error.to_string(),
        },
    }
}

fn code_lookup_search_section(state: &RunState) -> SectionExecution {
    match state.search.as_ref() {
        Some(response) => SectionExecution::Success {
            message: Some(summarize_code_lookup_search(response)),
        },
        None => SectionExecution::Skipped {
            reason: Some(
                "semantic_search has not completed; skipping code_lookup search snapshot"
                    .to_string(),
            ),
        },
    }
}

async fn context_bundle_section(config: &RunConfig, state: &mut RunState) -> SectionExecution {
    let Some(target) = resolve_bundle_target(config, state) else {
        return SectionExecution::Skipped {
            reason: Some("no candidate file available for context_bundle".to_string()),
        };
    };

    match run_context_bundle(config, &target).await {
        Ok(response) => {
            let message = summarize_context_bundle(&response);
            if config.verbose {
                dump_json("context_bundle response", &response);
            }
            state.bundle = Some(response);
            SectionExecution::Success {
                message: Some(message),
            }
        }
        Err(error) => SectionExecution::Failed {
            error: error.to_string(),
        },
    }
}

fn code_lookup_bundle_section(state: &RunState) -> SectionExecution {
    match state.bundle.as_ref() {
        Some(response) => SectionExecution::Success {
            message: Some(summarize_code_lookup_bundle(response)),
        },
        None => SectionExecution::Skipped {
            reason: Some(
                "context_bundle has not completed; skipping code_lookup bundle snapshot"
                    .to_string(),
            ),
        },
    }
}

async fn index_status_section(config: &RunConfig) -> SectionExecution {
    match run_index_status(config).await {
        Ok(response) => {
            let message = summarize_index_status(&response);
            if config.verbose {
                dump_json("index_status response", &response);
            }
            SectionExecution::Success {
                message: Some(message),
            }
        }
        Err(error) => SectionExecution::Failed {
            error: error.to_string(),
        },
    }
}

async fn repository_timeline_section(config: &RunConfig, state: &mut RunState) -> SectionExecution {
    match run_repository_timeline(config).await {
        Ok(response) => {
            let message = summarize_repository_timeline(&response);
            if config.verbose {
                dump_json("repository_timeline response", &response);
            }

            if config.include_diffs {
                if let Some(entry) = response.entries.first() {
                    match run_repository_timeline_entry(config, &entry.sha).await {
                        Ok(detail) => {
                            if config.verbose {
                                dump_json("repository_timeline_entry response", &detail);
                            }
                            println!("{}", summarize_repository_timeline_entry(&detail));
                        }
                        Err(error) => println!(
                            "repository_timeline_entry failed for {}: {}",
                            entry.sha, error
                        ),
                    }
                } else {
                    println!("repository_timeline_entry skipped: no commits present in response");
                }
            }

            state.timeline = Some(response);

            SectionExecution::Success {
                message: Some(message),
            }
        }
        Err(error) => SectionExecution::Failed {
            error: error.to_string(),
        },
    }
}

async fn run_ingest(config: &RunConfig) -> Result<IngestResponse, IngestError> {
    let params = IngestParams {
        root: Some(config.root.to_string_lossy().to_string()),
        include: None,
        exclude: None,
        database_name: config.database.clone(),
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
    config: &RunConfig,
) -> Result<SemanticSearchResponse, SemanticSearchError> {
    let params = SemanticSearchParams {
        root: Some(config.root.to_string_lossy().to_string()),
        query: config.query.clone(),
        database_name: config.database.clone(),
        limit: Some(config.limit),
        model: None,
        language: None,
        path_prefix: None,
        path_contains: None,
        classification: None,
        summary_mode: Some(SummaryMode::Brief),
        max_context_before: Some(1),
        max_context_after: Some(1),
    };

    semantic_search(params).await
}

async fn run_context_bundle(
    config: &RunConfig,
    file: &str,
) -> Result<ContextBundleResponse, ContextBundleError> {
    let params = ContextBundleParams {
        root: Some(config.root.to_string_lossy().to_string()),
        database_name: config.database.clone(),
        file: file.to_string(),
        symbol: None,
        max_snippets: Some(config.max_snippets),
        max_neighbors: Some(config.max_neighbors),
        budget_tokens: Some(config.budget_tokens),
        ranges: None,
        focus_line: None,
    };

    context_bundle(params).await
}

async fn run_index_status(config: &RunConfig) -> Result<IndexStatusResponse, IndexStatusError> {
    let params = IndexStatusParams {
        root: Some(config.root.to_string_lossy().to_string()),
        database_name: config.database.clone(),
        history_limit: Some(5),
    };

    get_index_status(params).await
}

async fn run_repository_timeline(
    config: &RunConfig,
) -> Result<RepositoryTimelineResponse, RepositoryTimelineError> {
    let params = RepositoryTimelineParams {
        root: Some(config.root.to_string_lossy().to_string()),
        database_name: config.database.clone(),
        branch: None,
        limit: Some(5),
        since: None,
        include_merges: Some(true),
        include_file_stats: Some(true),
        include_diffs: Some(config.include_diffs),
        paths: None,
        diff_pattern: None,
    };

    repository_timeline(params).await
}

async fn run_repository_timeline_entry(
    config: &RunConfig,
    commit_sha: &str,
) -> Result<RepositoryTimelineEntryLookupResponse, RepositoryTimelineError> {
    let params = RepositoryTimelineEntryLookupParams {
        root: Some(config.root.to_string_lossy().to_string()),
        database_name: config.database.clone(),
        commit_sha: commit_sha.to_string(),
    };

    repository_timeline_entry_detail(params).await
}

fn resolve_bundle_target(config: &RunConfig, state: &RunState) -> Option<String> {
    let mut candidates = Vec::new();

    if let Some(explicit) = &config.bundle_file {
        candidates.push(explicit.clone());
    }

    if let Some(search) = state.search.as_ref() {
        for result in &search.results {
            candidates.push(result.path.clone());
        }
    }

    candidates.push("rust-migration.md".to_string());
    candidates.push("README.md".to_string());

    let mut seen = HashSet::new();
    for candidate in candidates {
        if let Some(normalized) = normalize_candidate(&config.root, &candidate) {
            if seen.insert(normalized.clone()) {
                return Some(normalized);
            }
        }
    }

    None
}

fn normalize_candidate(root: &Path, candidate: &str) -> Option<String> {
    let candidate_path = PathBuf::from(candidate);
    let absolute = if candidate_path.is_absolute() {
        candidate_path
    } else {
        root.join(&candidate_path)
    };

    if !absolute.exists() {
        return None;
    }

    let relative = absolute.strip_prefix(root).unwrap_or(absolute.as_path());
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn summarize_ingest(response: &IngestResponse) -> String {
    let mut summary = format!(
        "ingest_codebase: {} file(s), {} embedded chunk(s), db={}",
        response.ingested_file_count, response.embedded_chunk_count, response.database_path
    );

    if let Some(reused) = response.reused_file_count {
        summary.push_str(&format!(", reused {} cached file(s)", reused));
    }

    summary
}

fn summarize_code_lookup_search(response: &SemanticSearchResponse) -> String {
    let mut lines = vec![format!(
        "code_lookup (search): mirrored {} semantic_search result(s)",
        response.results.len()
    )];
    lines.extend(suggestion_lines(response));
    lines.join("\n")
}

fn suggestion_lines(response: &SemanticSearchResponse) -> Vec<String> {
    if response.suggested_tools.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    lines.push("suggested tool chain:".to_string());

    for suggestion in response.suggested_tools.iter().take(3) {
        let description = suggestion.description.as_deref().unwrap_or("search match");
        let mut line = format!(
            "  {}#{} -> {} (score {:.2})",
            suggestion.tool, suggestion.rank, description, suggestion.score
        );

        if let Some(preview) = suggestion.preview.as_deref() {
            let cleaned = preview.replace('\n', " ").trim().to_string();
            if !cleaned.is_empty() {
                let mut snippet = String::new();
                let mut truncated = false;
                for (idx, ch) in cleaned.chars().enumerate() {
                    if idx >= 80 {
                        truncated = true;
                        break;
                    }
                    snippet.push(ch);
                }
                if truncated {
                    snippet.push_str("...");
                }
                line.push_str(" — ");
                line.push_str(&snippet);
            }
        }

        lines.push(line);
    }

    lines
}

fn summarize_context_bundle(response: &ContextBundleResponse) -> String {
    let mut lines = vec![format!(
        "context_bundle: {} definition(s), {} snippet(s), {} related symbol(s)",
        response.definitions.len(),
        response.snippets.len(),
        response.related.len()
    )];

    if let Some(focus) = response
        .focus_definition
        .as_ref()
        .or_else(|| response.definitions.first())
    {
        lines.push(format!("focus symbol: {} ({})", focus.name, focus.kind));
    }

    lines.push(format!("file: {}", response.file.path));
    if let Some(brief) = &response.file.brief {
        lines.push(format!("brief: {}", brief));
    }
    lines.push(format!(
        "tokens: {} used of {} ({} unused), summaries={}, excerpts={}",
        response.usage.used_tokens,
        response.usage.budget_tokens,
        response.usage.remaining_tokens,
        response.usage.summary_snippets,
        response.usage.excerpt_snippets
    ));
    if response.usage.cache_hit {
        lines.push("cache-hit: true".to_string());
    }
    lines.join("\n")
}

fn summarize_code_lookup_bundle(response: &ContextBundleResponse) -> String {
    format!(
        "code_lookup (bundle): reused bundle for {}, {} definition(s), cache_hit={}, tokens {} used/{}",
        response.file.path,
        response.definitions.len(),
        response.usage.cache_hit,
        response.usage.used_tokens,
        response.usage.budget_tokens
    )
}

fn summarize_index_status(response: &IndexStatusResponse) -> String {
    format!(
        "index_status: db_exists={}, total_files={}, total_chunks={}, is_stale={}",
        response.database_exists, response.total_files, response.total_chunks, response.is_stale
    )
}

fn summarize_repository_timeline(response: &RepositoryTimelineResponse) -> String {
    let mut lines = vec![format!(
        "repository_timeline: {} commit(s), merge_commits={}, total_insertions={}, total_deletions={}",
        response.total_commits,
        response.merge_commits,
        response.total_insertions,
        response.total_deletions
    )];

    if let Some(first) = response.entries.first() {
        lines.push(format!("latest commit: {} {}", first.sha, first.subject));
    }

    lines.join("\n")
}

fn summarize_repository_timeline_entry(response: &RepositoryTimelineEntryLookupResponse) -> String {
    let diff_len = response.diff.as_ref().map(|diff| diff.len()).unwrap_or(0);
    format!(
        "repository_timeline_entry: commit {} diff_bytes={}",
        response.entry.sha, diff_len
    )
}

fn dump_json<T: serde::Serialize>(label: &str, value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(output) => println!("{label} =>\n{output}"),
        Err(error) => eprintln!("failed to render {label} as JSON: {error}"),
    }
}
