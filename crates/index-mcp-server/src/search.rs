use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::task::JoinError;

use crate::ann::{self, ANN_META_BASENAME_KEY};
#[cfg(test)]
use crate::embedding::build_mock_backend;
use crate::embedding::{
    build_candle_backend, build_fastembed_backend, get_or_create_embedding_runner, EmbeddingHandle,
};
use crate::index_status::DEFAULT_DB_FILENAME;
use crate::ingest::DEFAULT_EMBEDDING_MODEL;
use tracing::warn;

const DEFAULT_RESULT_LIMIT: usize = 6;
const DEFAULT_IDENTIFIER_LIMIT: usize = 3;
const MAX_RESULT_LIMIT: usize = 50;
const DEFAULT_CONTEXT_BEFORE: usize = 1;
const DEFAULT_CONTEXT_AFTER: usize = 1;
const MAX_CONTEXT_LINES: usize = 6;
const MAX_BRIEF_CONTENT_CHARS: usize = 240;
const MAX_BRIEF_CONTEXT_CHARS: usize = 160;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchParams {
    #[serde(default)]
    pub root: Option<String>,
    pub query: String,
    #[serde(default)]
    pub database_name: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub path_contains: Option<String>,
    #[serde(default)]
    pub classification: Option<Classification>,
    #[serde(default)]
    pub summary_mode: Option<SummaryMode>,
    #[serde(default)]
    pub max_context_before: Option<u32>,
    #[serde(default)]
    pub max_context_after: Option<u32>,
    #[serde(default)]
    pub recent_hits: Option<Vec<SearchResultCoordinate>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultCoordinate {
    pub path: String,
    pub chunk_index: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Classification {
    Function,
    Comment,
    Code,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum SummaryMode {
    #[default]
    Brief,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SearchSource {
    Embedding,
    Lexical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum QueryIntent {
    Lexical,
    Embedding,
    Graph,
    Docstring,
    Hybrid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContextGoal {
    Debugging,
    ApiDiscovery,
    ChangeImpact,
    Navigation,
    GeneralUnderstanding,
}

#[derive(Debug, Clone, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SearchDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantized: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexical_latency_ms: Option<u128>,
    pub total_latency_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluated_chunk_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_intent: Option<QueryIntent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_goals: Vec<ContextGoal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexical_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ann_candidate_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clarification_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchMatch {
    pub path: String,
    pub chunk_index: i32,
    pub score: f32,
    pub normalized_score: f32,
    pub language: Option<String>,
    pub classification: Classification,
    pub content: String,
    pub embedding_model: String,
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
    pub source: SearchSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuggestedTool {
    pub tool: String,
    pub rank: u32,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchResponse {
    pub database_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_name: Option<String>,
    pub embedding_model: Option<String>,
    pub total_chunks: u64,
    pub evaluated_chunks: u64,
    pub results: Vec<SemanticSearchMatch>,
    pub summary_mode: SummaryMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_intent: Option<QueryIntent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_confidence: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_goals: Vec<ContextGoal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clarification_prompts: Vec<String>,
    #[serde(default)]
    pub suggested_tools: Vec<SuggestedTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<SearchDiagnostics>,
}

#[derive(Debug, Error)]
pub enum SemanticSearchError {
    #[error("failed to resolve workspace root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("embedding error: {0}")]
    Embedding(String),
    #[error("blocking task panicked: {0}")]
    Join(#[from] JoinError),
    #[error("multiple embedding models found ({available}). specify the desired model.")]
    MultipleModels { available: String },
    #[error("embedding model '{requested}' not found. available models: {available}")]
    ModelNotFound {
        requested: String,
        available: String,
    },
}

const MAX_EMBEDDING_CANDIDATES: usize = 256;

#[derive(Debug, Clone)]
struct QueryAnalysis {
    primary_intent: QueryIntent,
    lexical_weight: f32,
    embedding_weight: f32,
    confidence: f32,
    goals: Vec<ContextGoal>,
}

#[derive(Debug, Clone)]
struct SearchBudgetProfile {
    lexical_limit: usize,
    embedding_probe_factor: usize,
    lexical_probe_factor: usize,
    run_lexical: bool,
}

impl SearchBudgetProfile {
    fn for_analysis(
        base_limit: usize,
        lexical_hint: bool,
        identifier_like: bool,
        analysis: &QueryAnalysis,
    ) -> Self {
        let base_limit = base_limit.max(1);
        let embedding_probe_factor = if analysis.embedding_weight >= 0.65 {
            5
        } else if analysis.embedding_weight >= 0.45 {
            4
        } else if analysis.embedding_weight >= 0.30 {
            3
        } else {
            2
        };

        let lexical_probe_factor = if analysis.lexical_weight >= 0.5 {
            3
        } else if analysis.lexical_weight >= 0.2 {
            2
        } else {
            1
        };

        let run_lexical = lexical_hint || analysis.lexical_weight >= 0.08 || identifier_like;

        let mut lexical_limit = if run_lexical {
            ((base_limit as f32) * analysis.lexical_weight.max(0.12)).ceil() as usize
        } else {
            0
        };

        if identifier_like {
            lexical_limit = lexical_limit.max(DEFAULT_IDENTIFIER_LIMIT);
        }

        let lexical_ceiling = base_limit.max(DEFAULT_RESULT_LIMIT);
        lexical_limit = lexical_limit.min(lexical_ceiling);
        if run_lexical {
            lexical_limit = lexical_limit.max(1);
        }

        Self {
            lexical_limit,
            embedding_probe_factor,
            lexical_probe_factor,
            run_lexical,
        }
    }

    fn embedding_top_limit(&self, base_limit: usize) -> usize {
        let base_limit = base_limit.max(1);
        let candidate = base_limit
            .saturating_mul(self.embedding_probe_factor)
            .max(DEFAULT_RESULT_LIMIT);
        candidate.min(MAX_EMBEDDING_CANDIDATES)
    }

    fn ann_search_k(&self, base_limit: usize, lexical_results: usize) -> usize {
        let base_limit = base_limit.max(1);
        let embedding_probe = base_limit.saturating_mul(self.embedding_probe_factor);
        let lexical_influence = lexical_results.saturating_mul(self.lexical_probe_factor);
        let candidate = embedding_probe
            .max(lexical_influence)
            .max(base_limit)
            .min(MAX_EMBEDDING_CANDIDATES);
        candidate.max(32)
    }

    fn should_run_lexical(&self) -> bool {
        self.run_lexical && self.lexical_limit > 0
    }

    fn lexical_limit(&self) -> usize {
        self.lexical_limit
    }
}

fn analyze_query(query: &str, identifier_like: bool) -> QueryAnalysis {
    let trimmed = query.trim();
    let lower = trimmed.to_ascii_lowercase();
    let word_count = trimmed
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .count();
    let char_count = trimmed.chars().count();

    let mut lexical_weight: f32 = 0.0;
    let mut embedding_weight: f32 = 0.0;
    let mut graph_weight: f32 = 0.0;
    let mut docstring_weight: f32 = 0.0;

    if identifier_like {
        lexical_weight += 0.35;
        graph_weight += 0.45;
    }

    if trimmed.contains('"')
        || trimmed.contains('\'')
        || lower.contains("error")
        || lower.contains("exception")
        || lower.contains("failed")
        || lower.contains("panic")
        || lower.contains("stack trace")
    {
        lexical_weight += 0.5;
    }

    if trimmed.contains("::")
        || trimmed.contains("->")
        || trimmed.contains('.')
        || trimmed.contains('(')
        || lower.contains("protocol")
    {
        graph_weight += 0.35;
    }

    if word_count >= 6
        || char_count >= 48
        || trimmed.ends_with('?')
        || lower.contains("how ")
        || lower.contains("what ")
        || lower.contains("why ")
        || lower.contains("explain")
    {
        embedding_weight += 0.6;
    }

    if lower.contains("doc")
        || lower.contains("comment")
        || lower.contains("documentation")
        || trimmed.contains("///")
        || lower.contains("summary")
        || lower.contains("remarks")
    {
        docstring_weight += 0.7;
    }

    if lower.contains("todo") || lower.contains("fixme") {
        docstring_weight += 0.2;
        lexical_weight += 0.1;
    }

    if lower.contains("usage") || lower.contains("example") || lower.contains("guide") {
        embedding_weight += 0.3;
        docstring_weight += 0.2;
    }

    if lower.contains("api") || lower.contains("interface") || lower.contains("conformance") {
        graph_weight += 0.3;
        embedding_weight += 0.2;
    }

    if lexical_weight + embedding_weight + graph_weight + docstring_weight == 0.0 {
        if identifier_like {
            lexical_weight = 0.35;
            graph_weight = 0.45;
            embedding_weight = 0.2;
        } else if word_count <= 3 {
            lexical_weight = 0.4;
            embedding_weight = 0.35;
            graph_weight = 0.25;
        } else {
            embedding_weight = 0.55;
            lexical_weight = 0.3;
            graph_weight = 0.15;
        }
    }

    let total =
        (lexical_weight + embedding_weight + graph_weight + docstring_weight).max(f32::EPSILON);

    lexical_weight /= total;
    embedding_weight /= total;
    graph_weight /= total;
    docstring_weight /= total;

    let mut weights = [
        (QueryIntent::Lexical, lexical_weight),
        (QueryIntent::Embedding, embedding_weight),
        (QueryIntent::Graph, graph_weight),
        (QueryIntent::Docstring, docstring_weight),
    ];
    weights.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

    let mut primary_intent = weights[0].0;
    let mut confidence: f32 = weights[0].1;
    if weights.len() > 1 {
        let delta = confidence - weights[1].1;
        if delta < 0.15 {
            primary_intent = QueryIntent::Hybrid;
            confidence = confidence.max(weights[1].1);
        }
    }

    let goals = detect_context_goals(trimmed);

    QueryAnalysis {
        primary_intent,
        lexical_weight,
        embedding_weight,
        confidence: confidence.clamp(0.0, 1.0),
        goals,
    }
}

fn detect_context_goals(query: &str) -> Vec<ContextGoal> {
    let lower = query.to_ascii_lowercase();
    let mut goals = Vec::new();

    let mut push_unique = |goal| {
        if !goals.contains(&goal) {
            goals.push(goal);
        }
    };

    const DEBUG_TERMS: &[&str] = &[
        "error",
        "fail",
        "panic",
        "exception",
        "debug",
        "stack trace",
        "crash",
        "fix",
    ];
    if DEBUG_TERMS.iter().any(|term| lower.contains(term)) {
        push_unique(ContextGoal::Debugging);
    }

    const API_TERMS: &[&str] = &[
        "usage",
        "example",
        "api",
        "interface",
        "how do",
        "how to",
        "docs",
        "documentation",
    ];
    if API_TERMS.iter().any(|term| lower.contains(term)) {
        push_unique(ContextGoal::ApiDiscovery);
    }

    const CHANGE_TERMS: &[&str] = &[
        "impact",
        "refactor",
        "change",
        "diff",
        "upgrade",
        "regression",
        "breaking",
        "deprecate",
    ];
    if CHANGE_TERMS.iter().any(|term| lower.contains(term)) {
        push_unique(ContextGoal::ChangeImpact);
    }

    const NAV_TERMS: &[&str] = &[
        "where",
        "path",
        "file",
        "module",
        "navigate",
        "definition",
        "symbol",
        "jump",
    ];
    if NAV_TERMS.iter().any(|term| lower.contains(term)) {
        push_unique(ContextGoal::Navigation);
    }

    if goals.is_empty() {
        goals.push(ContextGoal::GeneralUnderstanding);
    }

    goals
}

pub async fn semantic_search(
    params: SemanticSearchParams,
) -> Result<SemanticSearchResponse, SemanticSearchError> {
    tokio::task::spawn_blocking(move || perform_semantic_search(params)).await?
}

#[derive(Default)]
struct FileEntry {
    lines: Option<Vec<String>>,
}

struct PendingMatch {
    id: String,
    path: String,
    chunk_index: i32,
    content: String,
    summary: Option<String>,
    symbol: Option<String>,
    identifier: Option<String>,
    source_type: Option<String>,
    metadata: Option<Value>,
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    embedding_model: String,
    score: f32,
    classification: Classification,
    language: Option<String>,
    source: SearchSource,
}

struct ChunkRow {
    id: String,
    path: String,
    chunk_index: i32,
    content: String,
    summary: Option<String>,
    symbol: Option<String>,
    identifier: Option<String>,
    source_type: Option<String>,
    language: Option<String>,
    metadata_raw: Option<String>,
    embedding_blob: Vec<u8>,
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    embedding_model: String,
}

fn perform_semantic_search(
    params: SemanticSearchParams,
) -> Result<SemanticSearchResponse, SemanticSearchError> {
    let SemanticSearchParams {
        root,
        query,
        database_name,
        limit,
        model,
        language,
        path_prefix,
        path_contains,
        classification,
        summary_mode,
        max_context_before,
        max_context_after,
        recent_hits,
    } = params;

    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return Ok(empty_response("", None, None));
    }

    let summary_mode = summary_mode.unwrap_or_default();
    let normalized_limit = normalize_limit(limit);
    let identifier_query = is_identifier_query(trimmed_query);
    let lexical_hint = should_run_lexical(trimmed_query);
    let query_analysis = analyze_query(trimmed_query, identifier_query);
    let budget_profile = SearchBudgetProfile::for_analysis(
        normalized_limit,
        lexical_hint,
        identifier_query,
        &query_analysis,
    );
    let should_run_lexical_query = budget_profile.should_run_lexical();
    let lexical_budget = if should_run_lexical_query {
        budget_profile.lexical_limit()
    } else {
        0
    };
    let embedding_candidate_limit = budget_profile.embedding_top_limit(normalized_limit);
    let language_filter = language.map(|value| value.to_lowercase());
    let context_before_lines = max_context_before
        .map(|value| value.min(MAX_CONTEXT_LINES as u32) as usize)
        .unwrap_or(DEFAULT_CONTEXT_BEFORE);
    let context_after_lines = max_context_after
        .map(|value| value.min(MAX_CONTEXT_LINES as u32) as usize)
        .unwrap_or(DEFAULT_CONTEXT_AFTER);

    let mut seen_hits: HashSet<(String, i32)> = recent_hits
        .unwrap_or_default()
        .into_iter()
        .map(|hit| (hit.path, hit.chunk_index))
        .collect();

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;
    let database_name_value = database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string());
    let db_path = absolute_root.join(&database_name_value);
    let ann_dir = ann::ann_directory(&db_path);
    let db_path_string = db_path.to_string_lossy().to_string();

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(SemanticSearchError::Sqlite)?;

    let total_chunks: u64 = conn
        .query_row("SELECT COUNT(*) FROM file_chunks", [], |row| row.get(0))
        .unwrap_or(0);

    if total_chunks == 0 {
        return Ok(empty_response(
            &db_path_string,
            Some(database_name_value),
            model,
        ));
    }

    let available_models = available_embedding_models(&conn)?;
    let requested_model = resolve_requested_model(model.clone(), &available_models)?;

    let total_timer = Instant::now();
    let context_goals = query_analysis.goals.clone();
    let intent_confidence = query_analysis.confidence;

    let mut diagnostics = SearchDiagnostics {
        model: Some(requested_model.clone()),
        ..Default::default()
    };
    diagnostics.query_intent = Some(query_analysis.primary_intent);
    diagnostics.intent_confidence = Some(intent_confidence);
    diagnostics.context_goals = context_goals.clone();
    if should_run_lexical_query && lexical_budget > 0 {
        diagnostics.lexical_limit = Some(lexical_budget as u32);
    }
    if embedding_candidate_limit > 0 {
        diagnostics.embedding_limit = Some(embedding_candidate_limit as u32);
    }

    let backend_label_meta = load_meta_value(&conn, "embedding_backend");
    diagnostics.backend = backend_label_meta.clone();
    let backend_label = backend_label_meta.unwrap_or_else(|| "onnx".to_string());
    let ann_basename_meta = load_meta_value(&conn, ANN_META_BASENAME_KEY);

    if normalized_limit == 0 {
        diagnostics.total_latency_ms = total_timer.elapsed().as_millis();
        diagnostics.evaluated_chunk_count = Some(0);
        diagnostics.backend = load_meta_value(&conn, "embedding_backend");
        diagnostics.quantized =
            load_meta_value(&conn, "embedding_quantized").map(|value| value == "true");
        diagnostics.dimension = load_meta_value(&conn, "embedding_dimension")
            .and_then(|value| value.parse::<u32>().ok());

        return Ok(SemanticSearchResponse {
            database_path: db_path_string,
            database_name: Some(database_name_value),
            embedding_model: Some(requested_model),
            total_chunks,
            evaluated_chunks: 0,
            results: Vec::new(),
            summary_mode,
            query_intent: Some(query_analysis.primary_intent),
            intent_confidence: Some(intent_confidence),
            context_goals: context_goals.clone(),
            clarification_prompts: Vec::new(),
            suggested_tools: Vec::new(),
            diagnostics: Some(diagnostics),
        });
    }

    let mut lexical_matches: Vec<PendingMatch> = Vec::new();
    let mut lexical_latency_ms: Option<u128> = None;
    if should_run_lexical_query && lexical_budget > 0 {
        let lexical_timer = Instant::now();
        lexical_matches = collect_lexical_matches(
            &conn,
            trimmed_query,
            lexical_budget,
            path_prefix.as_deref(),
            path_contains.as_deref(),
            classification.as_ref(),
            language_filter.as_deref(),
        )?;
        lexical_latency_ms = Some(lexical_timer.elapsed().as_millis());
    }

    if !lexical_matches.is_empty() {
        let mut filtered = Vec::with_capacity(lexical_matches.len());
        for pending in lexical_matches.into_iter() {
            let key = (pending.path.clone(), pending.chunk_index);
            if seen_hits.contains(&key) {
                continue;
            }
            seen_hits.insert(key);
            filtered.push(pending);
        }
        lexical_matches = filtered;
    }

    let ann_search_k = budget_profile.ann_search_k(normalized_limit, lexical_matches.len());
    diagnostics.ann_candidate_count = Some(ann_search_k as u32);

    let skip_embedding =
        identifier_query && !lexical_matches.is_empty() && query_analysis.embedding_weight < 0.35;

    let mut embedding_matches: Vec<PendingMatch> = Vec::new();
    let mut evaluated_chunks: u64 = 0;
    let mut embedding_latency_ms: Option<u128> = None;

    if !skip_embedding && lexical_matches.len() < normalized_limit {
        let embedding_timer = Instant::now();
        let backend = if backend_label.eq_ignore_ascii_case("candle") {
            build_candle_backend(&requested_model, None)
                .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
        } else {
            #[cfg(test)]
            {
                if backend_label.eq_ignore_ascii_case("mock") {
                    build_mock_backend()
                        .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
                } else {
                    build_fastembed_backend(&requested_model)
                        .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
                }
            }
            #[cfg(not(test))]
            {
                build_fastembed_backend(&requested_model)
                    .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
            }
        };

        let embedder_handle = get_or_create_embedding_runner(&backend, &requested_model, None)
            .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?;

        let query_embedding = {
            let mut guard = embedder_handle
                .lock()
                .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?;
            guard
                .embed_query(trimmed_query)
                .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
        };

        let ann_index = if let Some(basename) = ann_basename_meta.as_ref() {
            ann::load_ann_index(&ann_dir, basename).ok().flatten()
        } else {
            None
        };

        let mut top_matches: Vec<PendingMatch> = Vec::new();
        let top_limit = embedding_candidate_limit.max(normalized_limit.max(DEFAULT_RESULT_LIMIT));
        let mut ann_used = false;

        if let Some(ann_index) = ann_index.as_ref() {
            let ef = ann_search_k.max(64);

            match ann_index.search(&query_embedding, ann_search_k, ef) {
                Ok(neighbours) => {
                    evaluated_chunks = neighbours.len() as u64;

                    let mut chunk_stmt = conn.prepare(
                        "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding, embedding_model, byte_start, byte_end, line_start, line_end FROM file_chunks WHERE id = ?1",
                    )?;

                    for neighbour in neighbours {
                        let idx = neighbour.d_id;
                        let Some(chunk_id) = ann_index.id_lookup.get(idx) else {
                            continue;
                        };

                        let chunk = match chunk_stmt.query_row(params![chunk_id], read_chunk_row) {
                            Ok(value) => value,
                            Err(_) => continue,
                        };

                        if let Some(pending) = chunk_to_pending(
                            &chunk,
                            &query_embedding,
                            &classification,
                            path_prefix.as_deref(),
                            path_contains.as_deref(),
                            language_filter.as_deref(),
                            &mut seen_hits,
                        ) {
                            insert_into_top_matches(&mut top_matches, pending, top_limit);
                        }
                    }

                    ann_used = true;
                }
                Err(error) => {
                    warn!(
                        ?error,
                        basename = ann_index.basename(),
                        "failed to query ANN index; falling back to brute-force search"
                    );
                }
            }
        }

        if !ann_used {
            let mut stmt = conn.prepare(
                "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding, embedding_model, byte_start, byte_end, line_start, line_end FROM file_chunks WHERE embedding_model = ?1",
            )?;
            let mut rows = stmt.query(params![&requested_model])?;

            while let Some(row) = rows.next()? {
                evaluated_chunks += 1;
                let chunk = read_chunk_row(row)?;
                if let Some(pending) = chunk_to_pending(
                    &chunk,
                    &query_embedding,
                    &classification,
                    path_prefix.as_deref(),
                    path_contains.as_deref(),
                    language_filter.as_deref(),
                    &mut seen_hits,
                ) {
                    insert_into_top_matches(&mut top_matches, pending, top_limit);
                }
            }
        }

        embedding_matches = top_matches.into_iter().rev().collect();
        embedding_latency_ms = Some(embedding_timer.elapsed().as_millis());
    }

    diagnostics.embedding_latency_ms = embedding_latency_ms;
    diagnostics.evaluated_chunk_count = Some(evaluated_chunks);
    let mut combined: Vec<PendingMatch> = Vec::new();
    let mut seen_chunk_ids: HashSet<String> = HashSet::new();
    let mut seen_symbol_keys: HashSet<String> = HashSet::new();

    for pending in lexical_matches.into_iter() {
        if should_keep_match(&mut seen_chunk_ids, &mut seen_symbol_keys, &pending) {
            combined.push(pending);
            if combined.len() >= normalized_limit {
                break;
            }
        }
    }

    if combined.len() < normalized_limit {
        for pending in embedding_matches.into_iter() {
            if should_keep_match(&mut seen_chunk_ids, &mut seen_symbol_keys, &pending) {
                combined.push(pending);
                if combined.len() >= normalized_limit {
                    break;
                }
            }
        }
    }

    let mut file_cache: HashMap<String, FileEntry> = HashMap::new();
    let mut file_stmt = conn.prepare("SELECT content FROM files WHERE path = ?1")?;
    let mut update_stmt =
        conn.prepare("UPDATE file_chunks SET hits = COALESCE(hits, 0) + 1 WHERE id = ?1")?;

    let mut results = Vec::new();
    for pending in combined.into_iter() {
        let PendingMatch {
            id,
            path,
            chunk_index,
            content,
            summary,
            symbol,
            identifier,
            source_type,
            metadata,
            byte_start,
            byte_end,
            line_start,
            line_end,
            embedding_model,
            score,
            classification,
            language,
            source,
        } = pending;

        let file_entry = load_file_entry(&mut file_cache, &absolute_root, &mut file_stmt, &path)?;
        let (context_before, context_after) = extract_context(
            file_entry.lines.as_ref(),
            line_start,
            line_end,
            context_before_lines,
            context_after_lines,
        );
        let focus_content = extract_focus_span(file_entry.lines.as_ref(), line_start, line_end);

        update_stmt.execute(params![&id])?;

        let mut base_content = focus_content.unwrap_or_else(|| content.clone());
        if base_content.trim().is_empty() {
            base_content = content.clone();
        }

        let final_content = match summary_mode {
            SummaryMode::Brief => trim_with_ellipsis(&base_content, MAX_BRIEF_CONTENT_CHARS),
            SummaryMode::Full => base_content,
        };

        let mut before_context = context_before;
        let mut after_context = context_after;
        if summary_mode == SummaryMode::Brief {
            before_context =
                before_context.map(|value| trim_with_ellipsis(&value, MAX_BRIEF_CONTEXT_CHARS));
            after_context =
                after_context.map(|value| trim_with_ellipsis(&value, MAX_BRIEF_CONTEXT_CHARS));
        }

        let normalized = match source {
            SearchSource::Embedding => normalize_score(score),
            SearchSource::Lexical => score.clamp(0.0, 1.0),
        };

        results.push(SemanticSearchMatch {
            path: path.clone(),
            chunk_index,
            score,
            normalized_score: normalized,
            language,
            classification,
            content: final_content,
            embedding_model,
            byte_start,
            byte_end,
            line_start,
            line_end,
            context_before: before_context,
            context_after: after_context,
            source,
            summary,
            symbol,
            identifier,
            source_type,
            metadata,
            confidence: normalized,
        });
    }

    let (clarification_prompts, clarification_reasons) = build_clarification_prompts(
        trimmed_query,
        &query_analysis,
        &context_goals,
        &results,
        normalized_limit,
        should_run_lexical_query,
    );

    diagnostics.lexical_latency_ms = lexical_latency_ms;
    diagnostics.total_latency_ms = total_timer.elapsed().as_millis();
    diagnostics.backend = load_meta_value(&conn, "embedding_backend");
    diagnostics.quantized =
        load_meta_value(&conn, "embedding_quantized").map(|value| value == "true");
    diagnostics.dimension =
        load_meta_value(&conn, "embedding_dimension").and_then(|value| value.parse::<u32>().ok());
    diagnostics.clarification_reasons = clarification_reasons.clone();

    Ok(SemanticSearchResponse {
        database_path: db_path_string,
        database_name: Some(database_name_value),
        embedding_model: Some(requested_model),
        total_chunks,
        evaluated_chunks,
        results,
        summary_mode,
        query_intent: Some(query_analysis.primary_intent),
        intent_confidence: Some(intent_confidence),
        context_goals,
        clarification_prompts,
        suggested_tools: Vec::new(),
        diagnostics: Some(diagnostics),
    })
}

fn empty_response(
    db_path: &str,
    database_name: Option<String>,
    model: Option<String>,
) -> SemanticSearchResponse {
    SemanticSearchResponse {
        database_path: db_path.to_string(),
        database_name,
        embedding_model: model,
        total_chunks: 0,
        evaluated_chunks: 0,
        results: Vec::new(),
        summary_mode: SummaryMode::Brief,
        query_intent: None,
        intent_confidence: None,
        context_goals: Vec::new(),
        clarification_prompts: Vec::new(),
        suggested_tools: Vec::new(),
        diagnostics: None,
    }
}

fn resolve_root(root: &str) -> Result<PathBuf, SemanticSearchError> {
    let candidate = PathBuf::from(root);
    if candidate.is_absolute() {
        return Ok(candidate);
    }

    let cwd = std::env::current_dir().map_err(|source| SemanticSearchError::InvalidRoot {
        path: root.to_string(),
        source,
    })?;
    Ok(cwd.join(candidate))
}

fn available_embedding_models(conn: &Connection) -> Result<Vec<String>, SemanticSearchError> {
    let mut stmt = conn.prepare("SELECT DISTINCT embedding_model FROM file_chunks")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.flatten().collect())
}

fn resolve_requested_model(
    requested: Option<String>,
    available: &[String],
) -> Result<String, SemanticSearchError> {
    if let Some(requested) = requested {
        if available.iter().any(|model| model == &requested) {
            Ok(requested)
        } else {
            Err(SemanticSearchError::ModelNotFound {
                requested,
                available: available.join(", "),
            })
        }
    } else if available.len() == 1 {
        Ok(available[0].clone())
    } else {
        Err(SemanticSearchError::MultipleModels {
            available: available.join(", "),
        })
    }
}

fn normalize_limit(limit: Option<u32>) -> usize {
    match limit {
        Some(0) => 0,
        Some(value) => value.min(MAX_RESULT_LIMIT as u32) as usize,
        None => DEFAULT_RESULT_LIMIT,
    }
}

fn blob_to_vec(blob: &[u8]) -> Vec<f32> {
    if !blob.len().is_multiple_of(4) {
        return Vec::new();
    }
    let count = blob.len() / 4;
    let mut values = Vec::with_capacity(count);
    for chunk in blob.chunks_exact(4) {
        values.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    values
}

fn read_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        path: row.get(1)?,
        chunk_index: row.get(2)?,
        content: row.get(3)?,
        summary: row.get(4)?,
        symbol: row.get(5)?,
        identifier: row.get(6)?,
        source_type: row.get(7)?,
        language: row.get(8)?,
        metadata_raw: row.get(9)?,
        embedding_blob: row.get(10)?,
        embedding_model: row.get(11)?,
        byte_start: row.get(12)?,
        byte_end: row.get(13)?,
        line_start: row.get(14)?,
        line_end: row.get(15)?,
    })
}

fn chunk_to_pending(
    chunk: &ChunkRow,
    query_embedding: &[f32],
    classification_filter: &Option<Classification>,
    path_prefix: Option<&str>,
    path_contains: Option<&str>,
    language_filter: Option<&str>,
    seen_hits: &mut HashSet<(String, i32)>,
) -> Option<PendingMatch> {
    let key = (chunk.path.clone(), chunk.chunk_index);
    if seen_hits.contains(&key) {
        return None;
    }

    let classification_value = classify_snippet(&chunk.content);
    if let Some(required) = classification_filter {
        if &classification_value != required {
            return None;
        }
    }

    if let Some(prefix) = path_prefix {
        if !chunk.path.starts_with(prefix) {
            return None;
        }
    }

    if let Some(fragment) = path_contains {
        if !chunk.path.contains(fragment) {
            return None;
        }
    }

    let detected_language = chunk
        .language
        .clone()
        .or_else(|| detect_language(&chunk.path));

    if let Some(required_lang) = language_filter {
        match detected_language.as_ref().map(|value| value.to_lowercase()) {
            Some(lang) if lang == required_lang => {}
            Some(_) => return None,
            None => return None,
        }
    }

    let chunk_embedding = blob_to_vec(&chunk.embedding_blob);
    if chunk_embedding.is_empty() {
        return None;
    }

    let score = dot_product(query_embedding, &chunk_embedding);
    let metadata_value = chunk
        .metadata_raw
        .as_ref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());

    seen_hits.insert(key);

    Some(PendingMatch {
        id: chunk.id.clone(),
        path: chunk.path.clone(),
        chunk_index: chunk.chunk_index,
        content: chunk.content.clone(),
        summary: chunk.summary.clone(),
        symbol: chunk.symbol.clone(),
        identifier: chunk.identifier.clone(),
        source_type: chunk.source_type.clone(),
        metadata: metadata_value,
        byte_start: chunk.byte_start,
        byte_end: chunk.byte_end,
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        embedding_model: chunk.embedding_model.clone(),
        score,
        classification: classification_value,
        language: detected_language,
        source: SearchSource::Embedding,
    })
}

pub(crate) fn create_embedding_runner(
    model_name: &str,
    backend_label: &str,
) -> Result<EmbeddingHandle, SemanticSearchError> {
    let backend = if backend_label.eq_ignore_ascii_case("candle") {
        build_candle_backend(model_name, None)
            .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
    } else {
        build_fastembed_backend(model_name)
            .map_err(|error| SemanticSearchError::Embedding(error.to_string()))?
    };

    get_or_create_embedding_runner(&backend, model_name, None)
        .map_err(|error| SemanticSearchError::Embedding(error.to_string()))
}

fn dot_product(query: &[f32], chunk: &[f32]) -> f32 {
    if query.len() != chunk.len() {
        return 0.0;
    }
    query.iter().zip(chunk.iter()).map(|(a, b)| a * b).sum()
}

fn insert_into_top_matches(matches: &mut Vec<PendingMatch>, candidate: PendingMatch, limit: usize) {
    if limit == 0 {
        return;
    }

    let idx = matches
        .iter()
        .position(|existing| existing.score > candidate.score)
        .unwrap_or(matches.len());
    matches.insert(idx, candidate);
    if matches.len() > limit {
        matches.remove(0);
    }
}

fn should_run_lexical(query: &str) -> bool {
    is_identifier_query(query)
        || query.contains('/')
        || query.contains('.')
        || query.contains('-')
        || query.contains(' ')
}

fn goals_contains(goals: &[ContextGoal], goal: ContextGoal) -> bool {
    goals.contains(&goal)
}

fn build_clarification_prompts(
    query: &str,
    analysis: &QueryAnalysis,
    goals: &[ContextGoal],
    results: &[SemanticSearchMatch],
    result_limit: usize,
    ran_lexical: bool,
) -> (Vec<String>, Vec<String>) {
    let mut prompts = Vec::new();
    let mut reasons = Vec::new();

    if results.is_empty() {
        prompts.push(format!(
            "No indexed snippets matched '{query}'. Try naming the file or insert unique keywords."
        ));
        reasons.push("no_results".to_string());
    } else {
        let top_confidence = results
            .iter()
            .map(|entry| entry.confidence)
            .fold(0.0, f32::max);
        if top_confidence < 0.35 {
            prompts.push(
                "Top matches are low confidence—specify the framework, module, or identifier you're targeting."
                    .to_string(),
            );
            reasons.push("low_confidence".to_string());
        }
        if results.len() < result_limit.saturating_sub(1)
            && result_limit >= 4
            && top_confidence < 0.65
        {
            prompts.push(
                "Few results matched. Narrow the scope by adding a directory, language filter, or symbol name."
                    .to_string(),
            );
            reasons.push("sparse_results".to_string());
        }
    }

    if goals_contains(goals, ContextGoal::Debugging) && ran_lexical && results.is_empty() {
        prompts.push(
            "Paste the exact error message or stack trace so the index can find the failing code."
                .to_string(),
        );
        reasons.push("debug_goal_no_hits".to_string());
    } else if goals_contains(goals, ContextGoal::ApiDiscovery)
        && analysis.embedding_weight > analysis.lexical_weight
    {
        prompts.push(
            "Mention the target SDK, module, or protocol to surface precise API definitions."
                .to_string(),
        );
        reasons.push("api_goal_disambiguation".to_string());
    }

    if prompts.is_empty()
        && analysis.primary_intent == QueryIntent::Hybrid
        && analysis.confidence < 0.4
    {
        prompts.push(
            "Clarify whether you're searching for code, documentation, or call graphs so the engine can prioritize appropriately."
                .to_string(),
        );
        reasons.push("hybrid_intent".to_string());
    }

    (prompts, reasons)
}

fn collect_lexical_matches(
    conn: &Connection,
    query: &str,
    limit: usize,
    path_prefix: Option<&str>,
    path_contains: Option<&str>,
    classification_filter: Option<&Classification>,
    language_filter: Option<&str>,
) -> Result<Vec<PendingMatch>, SemanticSearchError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let fetch_limit = (limit.saturating_mul(4)).max(limit);
    let like_pattern = format!("%{}%", escape_like_pattern(query));
    let identifier_mode = is_identifier_query(query);
    let core_identifier = query
        .rsplit(|c| [':', '.', '#'].contains(&c))
        .find(|token| !token.trim().is_empty())
        .map(|token| token.trim());

    let mut matches = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    if identifier_mode {
        let mut stmt = conn.prepare(
            "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
             FROM file_chunks \
             WHERE identifier = ?1 \
             ORDER BY hits ASC, chunk_index ASC \
             LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![query, fetch_limit as i64])?;
        while let Some(row) = rows.next()? {
            if matches.len() >= limit {
                return Ok(matches);
            }
            if push_lexical_match(
                &mut matches,
                &mut seen_ids,
                row,
                classification_filter,
                path_prefix,
                path_contains,
                language_filter,
            )? && matches.len() >= limit
            {
                return Ok(matches);
            }
        }

        if matches.len() < limit {
            let mut stmt = conn.prepare(
                "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
                 FROM file_chunks \
                 WHERE symbol = ?1 \
                 ORDER BY hits ASC, chunk_index ASC \
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![query, fetch_limit as i64])?;
            while let Some(row) = rows.next()? {
                if matches.len() >= limit {
                    return Ok(matches);
                }
                if push_lexical_match(
                    &mut matches,
                    &mut seen_ids,
                    row,
                    classification_filter,
                    path_prefix,
                    path_contains,
                    language_filter,
                )? && matches.len() >= limit
                {
                    return Ok(matches);
                }
            }
        }

        if matches.len() < limit {
            let mut stmt = conn.prepare(
                "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
                 FROM file_chunks \
                 WHERE identifier LIKE ?1 ESCAPE '\\' \
                 ORDER BY hits ASC, chunk_index ASC \
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![&like_pattern, fetch_limit as i64])?;
            while let Some(row) = rows.next()? {
                if matches.len() >= limit {
                    return Ok(matches);
                }
                if push_lexical_match(
                    &mut matches,
                    &mut seen_ids,
                    row,
                    classification_filter,
                    path_prefix,
                    path_contains,
                    language_filter,
                )? && matches.len() >= limit
                {
                    return Ok(matches);
                }
            }
        }

        if matches.len() < limit {
            let mut stmt = conn.prepare(
                "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
                 FROM file_chunks \
                 WHERE symbol LIKE ?1 ESCAPE '\\' \
                 ORDER BY hits ASC, chunk_index ASC \
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![&like_pattern, fetch_limit as i64])?;
            while let Some(row) = rows.next()? {
                if matches.len() >= limit {
                    return Ok(matches);
                }
                if push_lexical_match(
                    &mut matches,
                    &mut seen_ids,
                    row,
                    classification_filter,
                    path_prefix,
                    path_contains,
                    language_filter,
                )? && matches.len() >= limit
                {
                    return Ok(matches);
                }
            }
        }

        if matches.len() < limit {
            if let Some(token) = core_identifier {
                if token != query {
                    let token_like = format!("%{}%", escape_like_pattern(token));

                    let mut stmt = conn.prepare(
                        "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
                         FROM file_chunks \
                         WHERE identifier LIKE ?1 ESCAPE '\\' \
                         ORDER BY hits ASC, chunk_index ASC \
                         LIMIT ?2",
                    )?;
                    let mut rows = stmt.query(params![&token_like, fetch_limit as i64])?;
                    while let Some(row) = rows.next()? {
                        if matches.len() >= limit {
                            return Ok(matches);
                        }
                        if push_lexical_match(
                            &mut matches,
                            &mut seen_ids,
                            row,
                            classification_filter,
                            path_prefix,
                            path_contains,
                            language_filter,
                        )? && matches.len() >= limit
                        {
                            return Ok(matches);
                        }
                    }

                    if matches.len() < limit {
                        let mut stmt = conn.prepare(
                            "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
                             FROM file_chunks \
                             WHERE symbol LIKE ?1 ESCAPE '\\' \
                             ORDER BY hits ASC, chunk_index ASC \
                             LIMIT ?2",
                        )?;
                        let mut rows = stmt.query(params![&token_like, fetch_limit as i64])?;
                        while let Some(row) = rows.next()? {
                            if matches.len() >= limit {
                                return Ok(matches);
                            }
                            if push_lexical_match(
                                &mut matches,
                                &mut seen_ids,
                                row,
                                classification_filter,
                                path_prefix,
                                path_contains,
                                language_filter,
                            )? && matches.len() >= limit
                            {
                                return Ok(matches);
                            }
                        }
                    }

                    if matches.len() < limit {
                        let mut stmt = conn.prepare(
                            "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
                             FROM file_chunks \
                             WHERE content LIKE ?1 ESCAPE '\\' \
                             ORDER BY hits ASC \
                             LIMIT ?2",
                        )?;
                        let mut rows = stmt.query(params![&token_like, fetch_limit as i64])?;
                        while let Some(row) = rows.next()? {
                            if matches.len() >= limit {
                                return Ok(matches);
                            }
                            if push_lexical_match(
                                &mut matches,
                                &mut seen_ids,
                                row,
                                classification_filter,
                                path_prefix,
                                path_contains,
                                language_filter,
                            )? && matches.len() >= limit
                            {
                                return Ok(matches);
                            }
                        }
                    }
                }
            }
        }
    }

    if matches.len() < limit {
        let mut stmt = conn.prepare(
            "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding_model, byte_start, byte_end, line_start, line_end \
             FROM file_chunks \
             WHERE content LIKE ?1 ESCAPE '\\' \
             ORDER BY hits ASC \
             LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![&like_pattern, fetch_limit as i64])?;
        while let Some(row) = rows.next()? {
            if matches.len() >= limit {
                return Ok(matches);
            }
            if push_lexical_match(
                &mut matches,
                &mut seen_ids,
                row,
                classification_filter,
                path_prefix,
                path_contains,
                language_filter,
            )? && matches.len() >= limit
            {
                return Ok(matches);
            }
        }
    }

    Ok(matches)
}

fn escape_like_pattern(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn push_lexical_match(
    matches: &mut Vec<PendingMatch>,
    seen_ids: &mut HashSet<String>,
    row: &rusqlite::Row<'_>,
    classification_filter: Option<&Classification>,
    path_prefix: Option<&str>,
    path_contains: Option<&str>,
    language_filter: Option<&str>,
) -> Result<bool, SemanticSearchError> {
    let id: String = row.get(0)?;
    let path: String = row.get(1)?;
    let chunk_index: i32 = row.get(2)?;
    let content: String = row.get(3)?;
    let summary: Option<String> = row.get(4)?;
    let symbol: Option<String> = row.get(5)?;
    let identifier: Option<String> = row.get(6)?;
    let source_type: Option<String> = row.get(7)?;
    let stored_language: Option<String> = row.get(8)?;
    let metadata_raw: Option<String> = row.get(9)?;
    let embedding_model: String = row.get(10)?;
    let byte_start: Option<i64> = row.get(11)?;
    let byte_end: Option<i64> = row.get(12)?;
    let line_start: Option<i64> = row.get(13)?;
    let line_end: Option<i64> = row.get(14)?;

    let classification_value = classify_snippet(&content);
    if let Some(required) = classification_filter {
        if &classification_value != required {
            return Ok(false);
        }
    }

    if let Some(prefix) = path_prefix {
        if !path.starts_with(prefix) {
            return Ok(false);
        }
    }

    if let Some(fragment) = path_contains {
        if !path.contains(fragment) {
            return Ok(false);
        }
    }

    let detected_language = stored_language.clone().or_else(|| detect_language(&path));
    if let Some(required_lang) = language_filter {
        match detected_language.as_ref().map(|value| value.to_lowercase()) {
            Some(ref lang) if lang == required_lang => {}
            Some(_) => return Ok(false),
            None => return Ok(false),
        }
    }

    if !seen_ids.insert(id.clone()) {
        return Ok(false);
    }

    let metadata_value = metadata_raw
        .as_ref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());

    let penalty = matches.len() as f32 * 0.05;
    let mut score = 1.0 - penalty;
    if score < 0.0 {
        score = 0.0;
    }

    matches.push(PendingMatch {
        id,
        path,
        chunk_index,
        content,
        summary,
        symbol,
        identifier,
        source_type,
        metadata: metadata_value,
        byte_start,
        byte_end,
        line_start,
        line_end,
        embedding_model,
        score,
        classification: classification_value,
        language: detected_language,
        source: SearchSource::Lexical,
    });

    Ok(true)
}

fn load_meta_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn load_file_entry<'cache>(
    cache: &'cache mut HashMap<String, FileEntry>,
    root: &Path,
    stmt: &mut rusqlite::Statement<'_>,
    path: &str,
) -> Result<&'cache FileEntry, SemanticSearchError> {
    if !cache.contains_key(path) {
        let content: Option<String> = stmt
            .query_row(params![path], |row| row.get(0))
            .unwrap_or(None);

        let resolved_content = match content {
            Some(text) => Some(text),
            None => {
                let full_path = root.join(path);
                fs::read_to_string(&full_path).ok()
            }
        };

        let lines = resolved_content
            .as_ref()
            .map(|text| text.lines().map(|line| line.to_string()).collect());

        cache.insert(path.to_string(), FileEntry { lines });
    }

    Ok(cache.get(path).unwrap())
}

fn extract_context(
    lines: Option<&Vec<String>>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    before_padding: usize,
    after_padding: usize,
) -> (Option<String>, Option<String>) {
    let lines = match lines {
        Some(lines) => lines,
        None => return (None, None),
    };

    let start = line_start.unwrap_or(0).max(1) as usize;
    let end = line_end.unwrap_or(start as i64) as usize;

    if start == 0 {
        return (None, None);
    }

    let before_start = start.saturating_sub(1);
    let before_begin = before_start.saturating_sub(before_padding);
    let before = if before_start == 0 || before_begin >= lines.len() {
        None
    } else {
        let slice_end = before_start.min(lines.len());
        let slice_start = before_begin.min(slice_end);
        if slice_start < slice_end {
            Some(lines[slice_start..slice_end].join("\n"))
        } else {
            None
        }
    };

    let after_start = end.saturating_sub(1).saturating_add(1);
    let after_end = (after_start + after_padding).min(lines.len());
    let after = if after_start >= lines.len() {
        None
    } else if after_start < after_end {
        Some(lines[after_start..after_end].join("\n"))
    } else {
        None
    };

    (before, after)
}

fn extract_focus_span(
    lines: Option<&Vec<String>>,
    line_start: Option<i64>,
    line_end: Option<i64>,
) -> Option<String> {
    let lines = lines?;
    let start = line_start.unwrap_or(0).max(1) as usize;
    if start == 0 || start > lines.len() {
        return None;
    }

    let mut end = line_end.unwrap_or(line_start.unwrap_or(0)).max(1) as usize;
    if end < start {
        end = start;
    }
    end = end.min(lines.len());

    if end < start {
        return None;
    }

    let slice = &lines[start.saturating_sub(1)..end];
    if slice.is_empty() {
        None
    } else {
        Some(slice.join("\n"))
    }
}

fn build_symbol_key(
    path: &str,
    identifier: &Option<String>,
    symbol: &Option<String>,
) -> Option<String> {
    if let Some(identifier) = identifier.as_ref() {
        return Some(format!("{path}::identifier::{identifier}"));
    }
    if let Some(symbol) = symbol.as_ref() {
        return Some(format!("{path}::symbol::{symbol}"));
    }
    None
}

fn should_keep_match(
    seen_chunk_ids: &mut HashSet<String>,
    seen_symbol_keys: &mut HashSet<String>,
    pending: &PendingMatch,
) -> bool {
    if seen_chunk_ids.contains(&pending.id) {
        return false;
    }

    if let Some(symbol_key) = build_symbol_key(&pending.path, &pending.identifier, &pending.symbol)
    {
        if seen_symbol_keys.contains(&symbol_key) {
            return false;
        }
        seen_symbol_keys.insert(symbol_key);
    }

    seen_chunk_ids.insert(pending.id.clone());
    true
}

fn normalize_score(score: f32) -> f32 {
    ((score + 1.0) / 2.0).clamp(0.0, 1.0)
}

fn trim_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut truncated = String::new();
    for (idx, c) in text.chars().enumerate() {
        if idx >= max_chars.saturating_sub(1) {
            break;
        }
        truncated.push(c);
    }
    truncated.push('…');
    truncated
}

fn is_identifier_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.len() > 64 || trimmed.contains(char::is_whitespace) {
        return false;
    }

    trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '#'))
}

pub(crate) fn detect_language(path: &str) -> Option<String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "ts" | "tsx" => Some("TypeScript".to_string()),
        "js" | "jsx" | "mjs" | "cjs" => Some("JavaScript".to_string()),
        "json" => Some("JSON".to_string()),
        "py" => Some("Python".to_string()),
        "rs" => Some("Rust".to_string()),
        "go" => Some("Go".to_string()),
        "java" => Some("Java".to_string()),
        "rb" => Some("Ruby".to_string()),
        "php" => Some("PHP".to_string()),
        "swift" => Some("Swift".to_string()),
        "kt" => Some("Kotlin".to_string()),
        "cs" => Some("C#".to_string()),
        "cpp" | "cc" => Some("C++".to_string()),
        "c" => Some("C".to_string()),
        "h" => Some("C/C++ Header".to_string()),
        "hpp" => Some("C++ Header".to_string()),
        "md" => Some("Markdown".to_string()),
        "yml" | "yaml" => Some("YAML".to_string()),
        _ => None,
    }
}

fn classify_snippet(snippet: &str) -> Classification {
    let trimmed = snippet.trim();
    if trimmed.is_empty() {
        return Classification::Code;
    }

    let lines: Vec<&str> = trimmed.lines().collect();
    if !lines.is_empty() && lines.iter().all(|line| is_comment_line(line)) {
        return Classification::Comment;
    }

    let contains_func_token = trimmed.split_whitespace().any(|token| token == "func");
    let swift_initializer = trimmed.starts_with("init(")
        || trimmed.starts_with("init ")
        || trimmed.contains(" init(")
        || trimmed.contains(" init?(")
        || trimmed.contains(" init!(")
        || trimmed.contains(" convenience init")
        || trimmed.contains(" required init");
    let swift_deinitializer = trimmed.starts_with("deinit")
        || trimmed.starts_with("deinit ")
        || trimmed.contains(" deinit");

    if trimmed.contains("class ")
        || trimmed.contains("def ")
        || trimmed.contains("fn ")
        || trimmed.contains("function ")
        || trimmed.contains("=>")
        || contains_func_token
        || swift_initializer
        || swift_deinitializer
    {
        Classification::Function
    } else {
        Classification::Code
    }
}

fn is_comment_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("<!--")
}

pub fn summarize_semantic_search(payload: &SemanticSearchResponse) -> String {
    if payload.evaluated_chunks == 0 {
        return "Semantic search evaluated 0 chunks and returned 0 match(es).".to_string();
    }
    let model = payload
        .embedding_model
        .as_deref()
        .unwrap_or(DEFAULT_EMBEDDING_MODEL);
    let lexical_hits = payload
        .results
        .iter()
        .filter(|result| matches!(result.source, SearchSource::Lexical))
        .count();
    let mut summary = format!(
        "Semantic search scanned {} chunk(s) and returned {} match(es) (model {}).",
        payload.evaluated_chunks,
        payload.results.len(),
        model
    );

    if lexical_hits > 0 {
        summary.push_str(&format!(
            " {} lexical match(es) promoted ahead of semantic ranks.",
            lexical_hits
        ));
    }

    if let Some(top) = payload.results.first() {
        let location = match top.line_start {
            Some(line) if line > 0 => format!("{}#L{}", top.path, line),
            _ => top.path.clone(),
        };
        summary.push_str(&format!(
            " Top hit: {} (confidence {:.2}).",
            location, top.confidence
        ));
    }

    if let Some(suggestion) = payload.suggested_tools.first() {
        summary.push_str(&format!(
            " Suggested follow-up: run {} with focus on {} (score {:.2}).",
            suggestion.tool,
            suggestion
                .description
                .as_deref()
                .unwrap_or("selected search match"),
            suggestion.score
        ));
    }

    if !payload.clarification_prompts.is_empty() {
        summary.push_str(" Clarify: ");
        summary.push_str(&payload.clarification_prompts.join(" "));
    }

    if let Some(diag) = &payload.diagnostics {
        if let Some(latency) = diag.embedding_latency_ms {
            summary.push_str(&format!(" Embedding latency: {} ms.", latency));
        }
        if let Some(latency) = diag.lexical_latency_ms {
            summary.push_str(&format!(" Lexical latency: {} ms.", latency));
        }
        summary.push_str(&format!(" Total wall time: {} ms.", diag.total_latency_ms));
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use rusqlite::{params, Connection};
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn semantic_match(confidence: f32) -> SemanticSearchMatch {
        SemanticSearchMatch {
            path: "src/lib.rs".into(),
            chunk_index: 0,
            score: confidence,
            normalized_score: confidence,
            language: Some("Rust".into()),
            classification: Classification::Code,
            content: "fn sample() {}".into(),
            embedding_model: "mock".into(),
            byte_start: Some(0),
            byte_end: Some(16),
            line_start: Some(10),
            line_end: Some(12),
            context_before: None,
            context_after: None,
            source: SearchSource::Embedding,
            summary: None,
            symbol: None,
            identifier: None,
            source_type: None,
            metadata: None,
            confidence,
        }
    }

    fn vec_to_blob(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn sample_chunk(path: &str) -> ChunkRow {
        ChunkRow {
            id: "chunk-1".to_string(),
            path: path.to_string(),
            chunk_index: 0,
            content: "fn example() { 42; }".to_string(),
            summary: None,
            symbol: None,
            identifier: None,
            source_type: Some("code".to_string()),
            language: Some("rust".to_string()),
            metadata_raw: None,
            embedding_blob: vec_to_blob(&[1.0, 0.0]),
            embedding_model: DEFAULT_EMBEDDING_MODEL.to_string(),
            byte_start: Some(0),
            byte_end: Some(18),
            line_start: Some(1),
            line_end: Some(1),
        }
    }

    #[test]
    fn identifier_query_short_circuits_embedding() -> Result<()> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("default.sqlite");
        let conn = Connection::open(&db_path)?;

        conn.execute_batch(
            r#"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE files (
                path TEXT PRIMARY KEY,
                content TEXT
            );
            CREATE TABLE file_chunks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                summary TEXT,
                symbol TEXT,
                identifier TEXT,
                source_type TEXT,
                language TEXT,
                metadata TEXT,
                embedding BLOB NOT NULL,
                embedding_model TEXT NOT NULL,
                byte_start INTEGER,
                byte_end INTEGER,
                line_start INTEGER,
                line_end INTEGER,
                hits INTEGER DEFAULT 0
            );
            "#,
        )?;

        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('embedding_backend', 'onnx-quantized')",
            [],
        )?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('embedding_quantized', 'true')",
            [],
        )?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('embedding_dimension', '384')",
            [],
        )?;

        let file_path = "crates/index-mcp-server/src/service.rs";
        let chunk_content =
            "impl EnvironmentSnapshot { fn bundle_budget(&self) -> usize { 1024 } }";

        conn.execute(
            "INSERT INTO files (path, content) VALUES (?1, ?2)",
            params![file_path, chunk_content],
        )?;

        conn.execute(
            "INSERT INTO file_chunks (id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding, embedding_model, byte_start, byte_end, line_start, line_end, hits) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, NULL, ?9, ?10, ?11, ?12, ?13, ?14, 0)",
            params![
                "chunk-1",
                file_path,
                0,
                chunk_content,
                Some("bundle_budget".to_string()),
                Some("EnvironmentSnapshot::bundle_budget".to_string()),
                Some("EnvironmentSnapshot::bundle_budget".to_string()),
                Some("Rust".to_string()),
                vec![0u8; 4],
                DEFAULT_EMBEDDING_MODEL,
                0i64,
                chunk_content.len() as i64,
                1i64,
                3i64
            ],
        )?;

        let params = SemanticSearchParams {
            root: Some(temp_dir.path().to_string_lossy().to_string()),
            query: "EnvironmentSnapshot::bundle_budget".to_string(),
            database_name: Some("default.sqlite".to_string()),
            limit: Some(3),
            model: None,
            language: None,
            path_prefix: None,
            path_contains: None,
            classification: None,
            summary_mode: Some(SummaryMode::Brief),
            max_context_before: None,
            max_context_after: None,
            recent_hits: None,
        };

        let response = perform_semantic_search(params)?;
        let diagnostics = response.diagnostics.expect("diagnostics");

        assert!(diagnostics.embedding_latency_ms.is_none());
        assert_eq!(diagnostics.evaluated_chunk_count, Some(0));
        assert_eq!(response.evaluated_chunks, 0);
        assert_eq!(response.results.len(), 1);
        assert!(matches!(response.results[0].source, SearchSource::Lexical));
        assert_eq!(
            response.results[0].identifier.as_deref(),
            Some("EnvironmentSnapshot::bundle_budget")
        );
        assert!(diagnostics.lexical_latency_ms.is_some());

        Ok(())
    }

    #[test]
    fn chunk_to_pending_respects_filters() {
        let chunk = sample_chunk("src/lib.rs");
        let mut seen_hits = HashSet::new();
        let query_embedding = vec![1.0, 0.0];
        let pending = chunk_to_pending(
            &chunk,
            &query_embedding,
            &Some(Classification::Function),
            Some("src"),
            Some("lib"),
            Some("rust"),
            &mut seen_hits,
        )
        .expect("chunk should pass filters");

        assert_eq!(pending.path, "src/lib.rs");
        assert!(matches!(pending.classification, Classification::Function));
        assert!(seen_hits.contains(&(chunk.path.clone(), chunk.chunk_index)));
    }

    #[test]
    fn chunk_to_pending_rejects_duplicates_and_mismatched_paths() {
        let chunk = sample_chunk("src/lib.rs");
        let mut seen_hits = HashSet::new();
        let query_embedding = vec![1.0, 0.0];

        // First call inserts into the seen set.
        let first = chunk_to_pending(
            &chunk,
            &query_embedding,
            &None,
            None,
            None,
            None,
            &mut seen_hits,
        );
        assert!(first.is_some());

        // Second call should be filtered out due to seen_hits.
        let second = chunk_to_pending(
            &chunk,
            &query_embedding,
            &None,
            None,
            None,
            None,
            &mut seen_hits,
        );
        assert!(second.is_none());

        // Unknown path prefix should filter the chunk immediately.
        let mut fresh_seen = HashSet::new();
        let filtered = chunk_to_pending(
            &chunk,
            &query_embedding,
            &None,
            Some("tests"),
            None,
            None,
            &mut fresh_seen,
        );
        assert!(filtered.is_none());
        assert!(fresh_seen.is_empty());
    }

    #[test]
    fn clarification_prompts_trigger_for_empty_result_sets() {
        let analysis = analyze_query("alpha", false);
        let (prompts, reasons) = build_clarification_prompts("alpha", &analysis, &[], &[], 6, true);
        assert!(!prompts.is_empty());
        assert!(reasons.iter().any(|reason| reason == "no_results"));
    }

    #[test]
    fn clarification_prompts_skip_for_confident_results() {
        let analysis = analyze_query("beta", false);
        let result = semantic_match(0.92);
        let (prompts, reasons) =
            build_clarification_prompts("beta", &analysis, &[], &[result], 6, true);
        assert!(prompts.is_empty());
        assert!(reasons.is_empty());
    }

    #[test]
    fn classify_snippet_flags_swift_functions() {
        let snippet = "public func greet(name: String) -> String { return \"Hi\" }";
        assert_eq!(classify_snippet(snippet), Classification::Function);
    }

    #[test]
    fn classify_snippet_handles_swift_init() {
        let snippet = "public convenience init?(rawValue: String) { self.init() }";
        assert_eq!(classify_snippet(snippet), Classification::Function);
    }

    #[test]
    fn classify_snippet_handles_swift_deinit() {
        let snippet = "deinit { cleanup() }";
        assert_eq!(classify_snippet(snippet), Classification::Function);
    }

    #[test]
    fn classify_snippet_treats_comment_with_func_as_comment() {
        let snippet = "// func pretend() {}";
        assert_eq!(classify_snippet(snippet), Classification::Comment);
    }
}
