use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::task::JoinError;
use tracing::debug;

use crate::index_status::DEFAULT_DB_FILENAME;
use crate::ingest::DEFAULT_EMBEDDING_MODEL;

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

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchMatch {
    pub path: String,
    pub chunk_index: i32,
    pub score: f32,
    pub normalized_score: f32,
    pub confidence: f32,
    pub language: Option<String>,
    pub classification: Classification,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    pub source: SearchSource,
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SearchSource {
    Semantic,
    Lexical,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchDiagnostics {
    pub strategy: SearchStrategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexical_latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<EmbeddingModelInfo>,
    pub total_returned: usize,
    pub duplicates_removed: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingModelInfo {
    pub name: String,
    pub dimension: usize,
    pub backend: String,
    pub quantized: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SearchStrategy {
    SemanticOnly,
    LexicalOnly,
    Hybrid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum QueryIntent {
    NaturalLanguage,
    Symbol,
    FilePath,
    CodeFragment,
    Unknown,
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

pub async fn semantic_search(
    params: SemanticSearchParams,
) -> Result<SemanticSearchResponse, SemanticSearchError> {
    tokio::task::spawn_blocking(move || {
        perform_semantic_search(params, SearchExecution::SemanticOnly)
    })
    .await?
}

pub async fn adaptive_search(
    params: SemanticSearchParams,
    intent: QueryIntent,
) -> Result<SemanticSearchResponse, SemanticSearchError> {
    tokio::task::spawn_blocking(move || {
        perform_semantic_search(params, SearchExecution::Adaptive(intent))
    })
    .await?
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
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    embedding_model: Option<String>,
    score: f32,
    normalized_score: f32,
    confidence: f32,
    classification: Classification,
    language: Option<String>,
    symbol: Option<String>,
    metadata: Option<Value>,
    source: SearchSource,
}

enum SearchExecution {
    SemanticOnly,
    Adaptive(QueryIntent),
}

fn perform_semantic_search(
    params: SemanticSearchParams,
    execution: SearchExecution,
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
    } = params;

    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return Ok(empty_response(
            "",
            None,
            None,
            match execution {
                SearchExecution::SemanticOnly => SearchStrategy::SemanticOnly,
                SearchExecution::Adaptive(_) => SearchStrategy::LexicalOnly,
            },
        ));
    }

    let summary_mode = summary_mode.unwrap_or_default();
    let normalized_limit = normalize_limit(limit);
    let adaptive_limit = if limit.is_none() {
        let base = if is_identifier_query(trimmed_query) {
            DEFAULT_IDENTIFIER_LIMIT
        } else {
            DEFAULT_RESULT_LIMIT
        };
        base.min(normalized_limit)
    } else {
        normalized_limit
    };

    let language_filter = language.map(|value| value.to_lowercase());
    let context_before_lines = max_context_before
        .map(|value| value.min(MAX_CONTEXT_LINES as u32) as usize)
        .unwrap_or(DEFAULT_CONTEXT_BEFORE);
    let context_after_lines = max_context_after
        .map(|value| value.min(MAX_CONTEXT_LINES as u32) as usize)
        .unwrap_or(DEFAULT_CONTEXT_AFTER);

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;
    let database_name_value = database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string());
    let db_path = absolute_root.join(&database_name_value);
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
            match execution {
                SearchExecution::SemanticOnly => SearchStrategy::SemanticOnly,
                SearchExecution::Adaptive(_) => SearchStrategy::LexicalOnly,
            },
        ));
    }

    let available_models = available_embedding_models(&conn)?;
    let mut semantic_matches: Vec<PendingMatch> = Vec::new();
    let mut evaluated_chunks: u64 = 0;
    let mut embedding_latency_ms: Option<u128> = None;
    let mut embedding_dimension: Option<usize> = None;
    let mut embedding_model_output: Option<String> = None;
    let mut execution_strategy = SearchStrategy::LexicalOnly;

    let run_embedding = match execution {
        SearchExecution::SemanticOnly => true,
        SearchExecution::Adaptive(_) => !available_models.is_empty(),
    };

    if run_embedding {
        match resolve_requested_model(model.clone(), &available_models) {
            Ok(requested_model) => {
                let mut stmt = conn.prepare(
                    "SELECT id, path, chunk_index, content, embedding, embedding_model, byte_start, byte_end, line_start, line_end, symbol, metadata FROM file_chunks WHERE embedding_model = ?1",
                )?;
                let mut rows = stmt.query(params![&requested_model])?;
                let mut embedder = create_embedder(&requested_model)?;
                let mut cached_query: Option<(String, Vec<f32>)> = None;
                let embed_start = Instant::now();

                while let Some(row) = rows.next()? {
                    evaluated_chunks += 1;
                    let id: String = row.get(0)?;
                    let path: String = row.get(1)?;
                    let chunk_index: i32 = row.get(2)?;
                    let content: String = row.get(3)?;
                    let embedding_blob: Vec<u8> = row.get(4)?;
                    let embedding_model: String = row.get(5)?;
                    let byte_start: Option<i64> = row.get(6)?;
                    let byte_end: Option<i64> = row.get(7)?;
                    let line_start: Option<i64> = row.get(8)?;
                    let line_end: Option<i64> = row.get(9)?;
                    let symbol: Option<String> = row.get(10)?;
                    let metadata_raw: Option<String> = row.get(11)?;

                    let classification_value = classify_snippet(&content);
                    if let Some(required) = &classification {
                        if &classification_value != required {
                            continue;
                        }
                    }

                    if let Some(prefix) = &path_prefix {
                        if !path.starts_with(prefix) {
                            continue;
                        }
                    }

                    if let Some(fragment) = &path_contains {
                        if !path.contains(fragment) {
                            continue;
                        }
                    }

                    let detected_language = detect_language(&path);
                    if let Some(required_lang) = &language_filter {
                        match detected_language.as_ref().map(|value| value.to_lowercase()) {
                            Some(ref lang) if lang == required_lang => {}
                            Some(_) => continue,
                            None => continue,
                        }
                    }

                    let chunk_embedding = blob_to_vec(&embedding_blob);
                    if chunk_embedding.is_empty() {
                        continue;
                    }

                    if embedding_dimension.is_none() {
                        embedding_dimension = Some(chunk_embedding.len());
                    }

                    let query_embedding = if let Some((cached_text, cached_vector)) = &cached_query
                    {
                        if cached_text == trimmed_query {
                            cached_vector.clone()
                        } else {
                            let vector = embed_query(&mut embedder, trimmed_query)?;
                            cached_query = Some((trimmed_query.to_string(), vector.clone()));
                            vector
                        }
                    } else {
                        let vector = embed_query(&mut embedder, trimmed_query)?;
                        cached_query = Some((trimmed_query.to_string(), vector.clone()));
                        vector
                    };

                    let score = dot_product(&query_embedding, &chunk_embedding);
                    let normalized = normalize_score(score);

                    let metadata = metadata_raw
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<Value>(payload).ok());

                    insert_into_top_matches(
                        &mut semantic_matches,
                        PendingMatch {
                            id,
                            path,
                            chunk_index,
                            content,
                            byte_start,
                            byte_end,
                            line_start,
                            line_end,
                            embedding_model: Some(embedding_model.clone()),
                            score,
                            normalized_score: normalized,
                            confidence: normalized,
                            classification: classification_value,
                            language: detected_language,
                            symbol,
                            metadata,
                            source: SearchSource::Semantic,
                        },
                        adaptive_limit,
                    );
                }

                embedding_latency_ms = Some(embed_start.elapsed().as_millis());
                embedding_model_output = Some(requested_model);
                if !semantic_matches.is_empty() {
                    execution_strategy = SearchStrategy::SemanticOnly;
                }
            }
            Err(error) => match execution {
                SearchExecution::SemanticOnly => return Err(error),
                SearchExecution::Adaptive(_) => {
                    if available_models.is_empty() {
                        debug!("No embeddings available; falling back to lexical search");
                    } else {
                        return Err(error);
                    }
                }
            },
        }
    }

    let mut results = if semantic_matches.is_empty() {
        Vec::new()
    } else {
        finalize_pending_matches(
            &conn,
            &absolute_root,
            semantic_matches,
            summary_mode,
            context_before_lines,
            context_after_lines,
        )?
    };

    drop(conn);

    let mut lexical_latency_ms: Option<u128> = None;
    let mut lexical_matches: Vec<SemanticSearchMatch> = Vec::new();
    let mut lexical_evaluated: u64 = 0;

    let should_run_lexical = match execution {
        SearchExecution::SemanticOnly => false,
        SearchExecution::Adaptive(intent) => match intent {
            QueryIntent::NaturalLanguage => results.is_empty(),
            QueryIntent::Symbol | QueryIntent::FilePath => true,
            QueryIntent::CodeFragment => true,
            QueryIntent::Unknown => results.is_empty(),
        },
    };

    if should_run_lexical {
        let lex_start = Instant::now();
        let (matches, evaluated) = perform_lexical_search(
            &absolute_root,
            &db_path,
            &trimmed_query,
            summary_mode,
            context_before_lines,
            context_after_lines,
            adaptive_limit,
            language_filter.as_deref(),
            path_prefix.as_deref(),
            path_contains.as_deref(),
            classification.as_ref(),
        )?;
        lexical_matches = matches;
        lexical_evaluated = evaluated;
        lexical_latency_ms = Some(lex_start.elapsed().as_millis());

        if !lexical_matches.is_empty() {
            execution_strategy = if results.is_empty() {
                SearchStrategy::LexicalOnly
            } else {
                SearchStrategy::Hybrid
            };
        }
    }

    let (merged_results, duplicates_removed) =
        merge_search_results(results, lexical_matches, adaptive_limit);

    let embedding_info = embedding_model_output
        .as_ref()
        .map(|name| EmbeddingModelInfo {
            name: name.clone(),
            dimension: embedding_dimension.unwrap_or_default(),
            backend: "fastembed (ONNX)".to_string(),
            quantized: name.ends_with('Q') || name.contains("-q"),
        });

    Ok(SemanticSearchResponse {
        database_path: db_path_string,
        database_name: Some(database_name_value),
        embedding_model: embedding_model_output,
        total_chunks,
        evaluated_chunks: evaluated_chunks + lexical_evaluated,
        results: merged_results.clone(),
        summary_mode,
        suggested_tools: Vec::new(),
        diagnostics: Some(SearchDiagnostics {
            strategy: execution_strategy,
            embedding_latency_ms,
            lexical_latency_ms,
            embedding_model: embedding_info,
            total_returned: merged_results.len(),
            duplicates_removed,
        }),
    })
}

fn empty_response(
    db_path: &str,
    database_name: Option<String>,
    model: Option<String>,
    strategy: SearchStrategy,
) -> SemanticSearchResponse {
    SemanticSearchResponse {
        database_path: db_path.to_string(),
        database_name,
        embedding_model: model.clone(),
        total_chunks: 0,
        evaluated_chunks: 0,
        results: Vec::new(),
        summary_mode: SummaryMode::Brief,
        suggested_tools: Vec::new(),
        diagnostics: Some(SearchDiagnostics {
            strategy,
            embedding_latency_ms: None,
            lexical_latency_ms: None,
            embedding_model: model.map(|name| EmbeddingModelInfo {
                name,
                dimension: 0,
                backend: "fastembed (ONNX)".to_string(),
                quantized: false,
            }),
            total_returned: 0,
            duplicates_removed: 0,
        }),
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

fn create_embedder(model_name: &str) -> Result<TextEmbedding, SemanticSearchError> {
    let name = model_name.trim();
    let parsed = EmbeddingModel::from_str(name).map_err(|error| {
        SemanticSearchError::Embedding(format!("Unknown embedding model '{name}': {error}"))
    })?;
    let options = TextInitOptions::new(parsed).with_show_download_progress(false);

    TextEmbedding::try_new(options)
        .map_err(|error| SemanticSearchError::Embedding(error.to_string()))
}

fn embed_query(embedder: &mut TextEmbedding, text: &str) -> Result<Vec<f32>, SemanticSearchError> {
    embedder
        .embed(vec![text.to_string()], None)
        .map_err(|error| SemanticSearchError::Embedding(error.to_string()))
        .map(|mut vectors| vectors.pop().unwrap_or_default())
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

fn finalize_pending_matches(
    conn: &Connection,
    absolute_root: &Path,
    pending: Vec<PendingMatch>,
    summary_mode: SummaryMode,
    context_before_lines: usize,
    context_after_lines: usize,
) -> Result<Vec<SemanticSearchMatch>, SemanticSearchError> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let mut file_cache: HashMap<String, FileEntry> = HashMap::new();
    let mut file_stmt = conn.prepare("SELECT content FROM files WHERE path = ?1")?;
    let mut update_stmt =
        conn.prepare("UPDATE file_chunks SET hits = COALESCE(hits, 0) + 1 WHERE id = ?1")?;

    let mut results = Vec::new();
    for pending in pending.into_iter().rev() {
        let PendingMatch {
            id,
            path,
            chunk_index,
            content,
            byte_start,
            byte_end,
            line_start,
            line_end,
            embedding_model,
            score,
            normalized_score,
            confidence,
            classification,
            language,
            symbol,
            metadata,
            source,
        } = pending;

        let file_entry = load_file_entry(&mut file_cache, absolute_root, &mut file_stmt, &path)?;
        let (context_before, context_after) = extract_context(
            file_entry.lines.as_ref(),
            line_start,
            line_end,
            context_before_lines,
            context_after_lines,
        );

        update_stmt.execute(params![&id])?;

        let final_content = match summary_mode {
            SummaryMode::Brief => trim_with_ellipsis(&content, MAX_BRIEF_CONTENT_CHARS),
            SummaryMode::Full => content,
        };

        let mut before_context = context_before;
        let mut after_context = context_after;
        if summary_mode == SummaryMode::Brief {
            before_context =
                before_context.map(|value| trim_with_ellipsis(&value, MAX_BRIEF_CONTEXT_CHARS));
            after_context =
                after_context.map(|value| trim_with_ellipsis(&value, MAX_BRIEF_CONTEXT_CHARS));
        }

        results.push(SemanticSearchMatch {
            path: path.clone(),
            chunk_index,
            score,
            normalized_score,
            confidence,
            language,
            classification,
            content: final_content,
            embedding_model,
            symbol,
            metadata,
            source,
            byte_start,
            byte_end,
            line_start,
            line_end,
            context_before: before_context,
            context_after: after_context,
        });
    }

    Ok(results)
}

fn perform_lexical_search(
    absolute_root: &Path,
    db_path: &Path,
    query: &str,
    summary_mode: SummaryMode,
    context_before_lines: usize,
    context_after_lines: usize,
    limit: usize,
    language_filter: Option<&str>,
    path_prefix: Option<&str>,
    path_contains: Option<&str>,
    classification: Option<&Classification>,
) -> Result<(Vec<SemanticSearchMatch>, u64), SemanticSearchError> {
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(SemanticSearchError::Sqlite)?;

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut pending: Vec<PendingMatch> = Vec::new();
    let mut evaluated_rows: u64 = 0;

    if limit == 0 {
        return Ok((Vec::new(), evaluated_rows));
    }

    let normalized_query = query.trim();
    if normalized_query.is_empty() {
        return Ok((Vec::new(), evaluated_rows));
    }

    let language_filter = language_filter.map(|value| value.to_lowercase());

    let mut push_match = |candidate: PendingMatch| {
        if seen_ids.insert(candidate.id.clone()) {
            insert_into_top_matches(&mut pending, candidate, limit);
        }
    };

    {
        let mut stmt = conn.prepare(
            "SELECT id, path, chunk_index, content, embedding_model, byte_start, byte_end, line_start, line_end, symbol, metadata \
             FROM file_chunks WHERE symbol IS NOT NULL AND lower(symbol) = lower(?1) LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![normalized_query, limit as i64])?;
        while let Some(row) = rows.next()? {
            evaluated_rows += 1;
            let id: String = row.get(0)?;
            let path: String = row.get(1)?;
            let chunk_index: i32 = row.get(2)?;
            let content: String = row.get(3)?;
            let embedding_model: Option<String> = row.get::<_, Option<String>>(4)?;
            let byte_start: Option<i64> = row.get(5)?;
            let byte_end: Option<i64> = row.get(6)?;
            let line_start: Option<i64> = row.get(7)?;
            let line_end: Option<i64> = row.get(8)?;
            let symbol: Option<String> = row.get(9)?;
            let metadata_raw: Option<String> = row.get(10)?;

            if let Some(prefix) = path_prefix {
                if !path.starts_with(prefix) {
                    continue;
                }
            }
            if let Some(fragment) = path_contains {
                if !path.contains(fragment) {
                    continue;
                }
            }

            let detected_language = detect_language(&path);
            if let Some(required_lang) = &language_filter {
                match detected_language.as_ref().map(|value| value.to_lowercase()) {
                    Some(ref lang) if lang == required_lang => {}
                    Some(_) => continue,
                    None => continue,
                }
            }

            let classification_value = classify_snippet(&content);
            if let Some(required) = classification {
                if &classification_value != required {
                    continue;
                }
            }

            let metadata = metadata_raw
                .as_deref()
                .and_then(|payload| serde_json::from_str::<Value>(payload).ok());

            push_match(PendingMatch {
                id,
                path,
                chunk_index,
                content,
                byte_start,
                byte_end,
                line_start,
                line_end,
                embedding_model,
                score: 1.25,
                normalized_score: 1.0,
                confidence: 1.0,
                classification: classification_value,
                language: detected_language,
                symbol,
                metadata,
                source: SearchSource::Lexical,
            });
        }
    }

    let like_query = format!(
        "%{}%",
        sanitize_like_pattern(&normalized_query.to_lowercase())
    );
    let mut stmt = conn.prepare(
        "SELECT id, path, chunk_index, content, embedding_model, byte_start, byte_end, line_start, line_end, symbol, metadata \
         FROM file_chunks WHERE lower(content) LIKE ?1 LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![like_query, (limit.saturating_mul(3)) as i64])?;
    while let Some(row) = rows.next()? {
        evaluated_rows += 1;
        let id: String = row.get(0)?;
        let path: String = row.get(1)?;
        let chunk_index: i32 = row.get(2)?;
        let content: String = row.get(3)?;
        let embedding_model: Option<String> = row.get::<_, Option<String>>(4)?;
        let byte_start: Option<i64> = row.get(5)?;
        let byte_end: Option<i64> = row.get(6)?;
        let line_start: Option<i64> = row.get(7)?;
        let line_end: Option<i64> = row.get(8)?;
        let symbol: Option<String> = row.get(9)?;
        let metadata_raw: Option<String> = row.get(10)?;

        if let Some(prefix) = path_prefix {
            if !path.starts_with(prefix) {
                continue;
            }
        }
        if let Some(fragment) = path_contains {
            if !path.contains(fragment) {
                continue;
            }
        }

        let detected_language = detect_language(&path);
        if let Some(required_lang) = &language_filter {
            match detected_language.as_ref().map(|value| value.to_lowercase()) {
                Some(ref lang) if lang == required_lang => {}
                Some(_) => continue,
                None => continue,
            }
        }

        let classification_value = classify_snippet(&content);
        if let Some(required) = classification {
            if &classification_value != required {
                continue;
            }
        }

        let metadata = metadata_raw
            .as_deref()
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok());

        let confidence = lexical_confidence(normalized_query, &content);
        if confidence <= 0.0 {
            continue;
        }

        push_match(PendingMatch {
            id,
            path,
            chunk_index,
            content,
            byte_start,
            byte_end,
            line_start,
            line_end,
            embedding_model,
            score: confidence,
            normalized_score: confidence,
            confidence,
            classification: classification_value,
            language: detected_language,
            symbol,
            metadata,
            source: SearchSource::Lexical,
        });
    }

    let finalized = finalize_pending_matches(
        &conn,
        absolute_root,
        pending,
        summary_mode,
        context_before_lines,
        context_after_lines,
    )?;

    Ok((finalized, evaluated_rows))
}

fn merge_search_results(
    semantic: Vec<SemanticSearchMatch>,
    lexical: Vec<SemanticSearchMatch>,
    limit: usize,
) -> (Vec<SemanticSearchMatch>, usize) {
    if limit == 0 {
        return (Vec::new(), 0);
    }

    let mut seen: HashSet<(String, i32)> = HashSet::new();
    let mut merged: Vec<SemanticSearchMatch> = Vec::with_capacity(semantic.len() + lexical.len());
    let mut duplicates = 0usize;

    for item in semantic.into_iter().chain(lexical.into_iter()) {
        let key = (item.path.clone(), item.chunk_index);
        if seen.insert(key) {
            merged.push(item);
        } else {
            duplicates += 1;
        }
    }

    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| source_rank(&a.source).cmp(&source_rank(&b.source)))
    });

    if merged.len() > limit {
        merged.truncate(limit);
    }

    (merged, duplicates)
}

fn source_rank(source: &SearchSource) -> u8 {
    match source {
        SearchSource::Semantic => 0,
        SearchSource::Lexical => 1,
    }
}

fn sanitize_like_pattern(input: &str) -> String {
    let mut escaped = String::new();
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

fn lexical_confidence(query: &str, haystack: &str) -> f32 {
    if query.is_empty() || haystack.is_empty() {
        return 0.0;
    }

    let query_lower = query.to_lowercase();
    let haystack_lower = haystack.to_lowercase();
    if let Some(position) = haystack_lower.find(&query_lower) {
        let haystack_len = haystack_lower.len().max(1) as f32;
        let proximity = 1.0 - (position as f32 / haystack_len);
        let coverage = (query_lower.len() as f32 / haystack_len).min(1.0);
        (0.6 * proximity + 0.4 * coverage).clamp(0.05, 1.0)
    } else {
        0.0
    }
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

fn detect_language(path: &str) -> Option<String> {
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

    if trimmed.contains("class ")
        || trimmed.contains("def ")
        || trimmed.contains("fn ")
        || trimmed.contains("function ")
        || trimmed.contains("=>")
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

pub fn classify_query(query: &str) -> QueryIntent {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return QueryIntent::Unknown;
    }

    let has_whitespace = trimmed.chars().any(char::is_whitespace);
    let looks_like_path = trimmed.contains('/') || trimmed.contains('\\');
    if looks_like_path && !has_whitespace {
        return QueryIntent::FilePath;
    }

    if trimmed.contains("::") || trimmed.contains("->") {
        return QueryIntent::Symbol;
    }

    if trimmed.contains('(') || trimmed.contains('{') || trimmed.contains('}') {
        return QueryIntent::CodeFragment;
    }

    if !has_whitespace {
        if is_identifier_query(trimmed) {
            return QueryIntent::Symbol;
        }

        let alnum = trimmed
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '#'));
        if alnum {
            return QueryIntent::Symbol;
        }
    }

    if trimmed.split_whitespace().count() > 1 {
        return QueryIntent::NaturalLanguage;
    }

    QueryIntent::NaturalLanguage
}

pub fn summarize_semantic_search(payload: &SemanticSearchResponse) -> String {
    if payload.evaluated_chunks == 0 {
        return "Semantic search evaluated 0 chunks and returned 0 match(es).".to_string();
    }
    let model = payload
        .embedding_model
        .as_deref()
        .unwrap_or(DEFAULT_EMBEDDING_MODEL);
    let mut summary = format!(
        "Semantic search scanned {} chunk(s) and returned {} match(es) (model {}).",
        payload.evaluated_chunks,
        payload.results.len(),
        model
    );

    if let Some(top) = payload.results.first() {
        let location = match top.line_start {
            Some(line) if line > 0 => format!("{}#L{}", top.path, line),
            _ => top.path.clone(),
        };
        summary.push_str(&format!(
            " Top hit: {} (score {:.2}).",
            location, top.normalized_score
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

    summary
}
