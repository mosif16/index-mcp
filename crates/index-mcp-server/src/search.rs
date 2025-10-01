use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::JoinError;

use crate::index_status::DEFAULT_DB_FILENAME;
use crate::ingest::DEFAULT_EMBEDDING_MODEL;

const DEFAULT_RESULT_LIMIT: usize = 8;
const MAX_RESULT_LIMIT: usize = 50;
const CONTEXT_LINE_PADDING: usize = 2;

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
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Classification {
    Function,
    Comment,
    Code,
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
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSearchResponse {
    pub database_path: String,
    pub embedding_model: Option<String>,
    pub total_chunks: u64,
    pub evaluated_chunks: u64,
    pub results: Vec<SemanticSearchMatch>,
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
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    embedding_model: String,
    score: f32,
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
    } = params;

    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return Ok(empty_response("", None));
    }

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;
    let db_path =
        absolute_root.join(database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string()));
    let db_path_string = db_path.to_string_lossy().to_string();

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(SemanticSearchError::Sqlite)?;

    let total_chunks: u64 = conn
        .query_row("SELECT COUNT(*) FROM file_chunks", [], |row| row.get(0))
        .unwrap_or(0);

    if total_chunks == 0 {
        return Ok(empty_response(&db_path_string, model));
    }

    let available_models = available_embedding_models(&conn)?;
    let requested_model = resolve_requested_model(model, &available_models)?;

    let limit = normalize_limit(limit);
    let mut top_matches: Vec<PendingMatch> = Vec::new();
    let mut evaluated_chunks: u64 = 0;

    let mut stmt = conn.prepare(
        "SELECT id, path, chunk_index, content, embedding, embedding_model, byte_start, byte_end, line_start, line_end FROM file_chunks WHERE embedding_model = ?1",
    )?;

    let mut rows = stmt.query(params![&requested_model])?;

    let mut embedder = create_embedder(&requested_model)?;
    let mut cached_query: Option<(String, Vec<f32>)> = None;

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

        insert_into_top_matches(
            &mut top_matches,
            PendingMatch {
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
            },
            limit,
        );
    }

    let mut file_cache: HashMap<String, FileEntry> = HashMap::new();
    let mut file_stmt = conn.prepare("SELECT content FROM files WHERE path = ?1")?;
    let mut update_stmt =
        conn.prepare("UPDATE file_chunks SET hits = COALESCE(hits, 0) + 1 WHERE id = ?1")?;

    let mut results = Vec::new();
    for pending in top_matches.into_iter().rev() {
        let file_entry = load_file_entry(
            &mut file_cache,
            &absolute_root,
            &mut file_stmt,
            &pending.path,
        )?;
        let (context_before, context_after) = extract_context(
            file_entry.lines.as_ref(),
            pending.line_start,
            pending.line_end,
        );

        update_stmt.execute(params![&pending.id])?;

        results.push(SemanticSearchMatch {
            path: pending.path.clone(),
            chunk_index: pending.chunk_index,
            score: pending.score,
            normalized_score: normalize_score(pending.score),
            language: detect_language(&pending.path),
            classification: classify_snippet(&pending.content),
            content: pending.content,
            embedding_model: pending.embedding_model,
            byte_start: pending.byte_start,
            byte_end: pending.byte_end,
            line_start: pending.line_start,
            line_end: pending.line_end,
            context_before,
            context_after,
        });
    }

    Ok(SemanticSearchResponse {
        database_path: db_path_string,
        embedding_model: Some(requested_model),
        total_chunks,
        evaluated_chunks,
        results,
    })
}

fn empty_response(db_path: &str, model: Option<String>) -> SemanticSearchResponse {
    SemanticSearchResponse {
        database_path: db_path.to_string(),
        embedding_model: model,
        total_chunks: 0,
        evaluated_chunks: 0,
        results: Vec::new(),
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
    let mut models = Vec::new();
    for row in rows {
        if let Ok(model) = row {
            models.push(model);
        }
    }
    Ok(models)
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
        Some(value) if value == 0 => 0,
        Some(value) => value.min(MAX_RESULT_LIMIT as u32) as usize,
        None => DEFAULT_RESULT_LIMIT,
    }
}

fn blob_to_vec(blob: &[u8]) -> Vec<f32> {
    if blob.len() % 4 != 0 {
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
    let options = if name.eq_ignore_ascii_case(DEFAULT_EMBEDDING_MODEL) {
        TextInitOptions::default()
    } else {
        let parsed = EmbeddingModel::from_str(name).map_err(|error| {
            SemanticSearchError::Embedding(format!("Unknown embedding model '{name}': {error}"))
        })?;
        TextInitOptions::new(parsed)
    }
    .with_show_download_progress(false);

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
    let before_begin = before_start.saturating_sub(CONTEXT_LINE_PADDING);
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
    let after_end = (after_start + CONTEXT_LINE_PADDING).min(lines.len());
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

pub fn summarize_semantic_search(payload: &SemanticSearchResponse) -> String {
    if payload.evaluated_chunks == 0 {
        return "Semantic search evaluated 0 chunks and returned 0 match(es).".to_string();
    }
    let model = payload
        .embedding_model
        .as_deref()
        .unwrap_or(DEFAULT_EMBEDDING_MODEL);
    format!(
        "Semantic search scanned {} chunk(s) and returned {} match(es) (model {}).",
        payload.evaluated_chunks,
        payload.results.len(),
        model
    )
}
