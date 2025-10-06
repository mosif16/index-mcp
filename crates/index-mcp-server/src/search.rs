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
    let should_run_lexical_query = should_run_lexical(trimmed_query);
    let identifier_query = is_identifier_query(trimmed_query);
    let lexical_budget = if should_run_lexical_query {
        normalized_limit
    } else {
        DEFAULT_IDENTIFIER_LIMIT.min(normalized_limit)
    };
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

    let mut diagnostics = SearchDiagnostics {
        model: Some(requested_model.clone()),
        ..Default::default()
    };

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

    let skip_embedding = identifier_query && !lexical_matches.is_empty();

    let mut embedding_matches: Vec<PendingMatch> = Vec::new();
    let mut evaluated_chunks: u64 = 0;
    let mut embedding_latency_ms: Option<u128> = None;

    if !skip_embedding && lexical_matches.len() < normalized_limit {
        let embedding_timer = Instant::now();
        let mut stmt = conn.prepare(
            "SELECT id, path, chunk_index, content, summary, symbol, identifier, source_type, language, metadata, embedding, embedding_model, byte_start, byte_end, line_start, line_end FROM file_chunks WHERE embedding_model = ?1",
        )?;
        let mut rows = stmt.query(params![&requested_model])?;

        let mut embedder = create_embedder(&requested_model)?;
        let mut cached_query: Option<(String, Vec<f32>)> = None;
        let mut top_matches: Vec<PendingMatch> = Vec::new();

        while let Some(row) = rows.next()? {
            evaluated_chunks += 1;
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
            let embedding_blob: Vec<u8> = row.get(10)?;
            let embedding_model: String = row.get(11)?;
            let byte_start: Option<i64> = row.get(12)?;
            let byte_end: Option<i64> = row.get(13)?;
            let line_start: Option<i64> = row.get(14)?;
            let line_end: Option<i64> = row.get(15)?;

            let chunk_key = (path.clone(), chunk_index);
            if seen_hits.contains(&chunk_key) {
                continue;
            }

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

            let detected_language = stored_language.clone().or_else(|| detect_language(&path));
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

            let query_embedding = if let Some((cached_text, cached_vector)) = &cached_query {
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
            let metadata_value = metadata_raw
                .as_ref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok());

            insert_into_top_matches(
                &mut top_matches,
                PendingMatch {
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
                    source: SearchSource::Embedding,
                },
                normalized_limit.max(DEFAULT_RESULT_LIMIT),
            );
        }

        embedding_latency_ms = Some(embedding_timer.elapsed().as_millis());
        embedding_matches = top_matches.into_iter().rev().collect();
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

    diagnostics.lexical_latency_ms = lexical_latency_ms;
    diagnostics.total_latency_ms = total_timer.elapsed().as_millis();
    diagnostics.backend = load_meta_value(&conn, "embedding_backend");
    diagnostics.quantized =
        load_meta_value(&conn, "embedding_quantized").map(|value| value == "true");
    diagnostics.dimension =
        load_meta_value(&conn, "embedding_dimension").and_then(|value| value.parse::<u32>().ok());

    Ok(SemanticSearchResponse {
        database_path: db_path_string,
        database_name: Some(database_name_value),
        embedding_model: Some(requested_model),
        total_chunks,
        evaluated_chunks,
        results,
        summary_mode,
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

pub(crate) fn create_embedder(model_name: &str) -> Result<TextEmbedding, SemanticSearchError> {
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

fn should_run_lexical(query: &str) -> bool {
    is_identifier_query(query)
        || query.contains('/')
        || query.contains('.')
        || query.contains('-')
        || query.contains(' ')
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
    use tempfile::tempdir;

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
}
