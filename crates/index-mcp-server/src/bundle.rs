use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use once_cell::sync::Lazy;
use regex::Regex;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::task::JoinError;

use crate::index_status::DEFAULT_DB_FILENAME;
use crate::search::create_embedding_runner;

const DEFAULT_SNIPPET_LIMIT: usize = 3;
const MAX_SNIPPET_LIMIT: usize = 10;
const DEFAULT_NEIGHBOR_LIMIT: usize = 12;
const MAX_NEIGHBOR_LIMIT: usize = 50;
const DEFAULT_TOKEN_BUDGET: usize = 3_000;
const FOCUS_CONTEXT_RADIUS: u32 = 25;
const SUMMARY_CHAR_LIMIT: usize = 220;
const EXCERPT_TOKEN_LIMIT: usize = 320;
const MIN_SUMMARY_TOKEN_FLOOR: usize = 1;
const BUNDLE_CACHE_CAPACITY: usize = 32;

static CONTEXT_BUNDLE_CACHE: Lazy<Mutex<BundleCache>> =
    Lazy::new(|| Mutex::new(BundleCache::new(BUNDLE_CACHE_CAPACITY)));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BundleCacheKey {
    database_path: String,
    file_path: String,
    file_hash: String,
    symbol: Option<(String, Option<String>)>,
    ranges: Vec<(u32, u32)>,
    focus_line: Option<u32>,
    max_snippets: usize,
    budget_tokens: usize,
    max_neighbors: usize,
    query_fingerprint: Option<String>,
}

#[derive(Debug)]
struct BundleCache {
    entries: HashMap<BundleCacheKey, ContextBundleResponse>,
    order: Vec<BundleCacheKey>,
    capacity: usize,
}

impl BundleCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            capacity,
        }
    }

    fn get(&mut self, key: &BundleCacheKey) -> Option<ContextBundleResponse> {
        if let Some(mut response) = self.entries.get(key).cloned() {
            self.promote(key);
            response.usage.cache_hit = true;
            return Some(response);
        }
        None
    }

    fn put(&mut self, key: BundleCacheKey, value: ContextBundleResponse) {
        let mut value = value;
        value.usage.cache_hit = false;
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), value);
            self.promote(&key);
            return;
        }

        if self.entries.len() >= self.capacity {
            if let Some(oldest) = self.order.first().cloned() {
                self.entries.remove(&oldest);
                self.order.remove(0);
            }
        }

        self.order.push(key.clone());
        self.entries.insert(key, value);
    }

    fn promote(&mut self, key: &BundleCacheKey) {
        if let Some(position) = self.order.iter().position(|existing| existing == key) {
            let tracked = self.order.remove(position);
            self.order.push(tracked);
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextBundleParams {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub database_name: Option<String>,
    pub file: String,
    #[serde(default)]
    pub symbol: Option<SymbolSelector>,
    #[serde(default)]
    pub max_snippets: Option<u32>,
    #[serde(default)]
    pub max_neighbors: Option<u32>,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
    #[serde(default)]
    pub ranges: Option<Vec<LineRange>>,
    #[serde(default)]
    pub focus_line: Option<u32>,
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SymbolSelector {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LineRange {
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContextBundleResponse {
    pub database_path: String,
    pub file: BundleFileMetadata,
    pub definitions: Vec<BundleDefinition>,
    pub focus_definition: Option<BundleDefinition>,
    pub related: Vec<BundleEdgeNeighbor>,
    pub snippets: Vec<BundleSnippet>,
    pub latest_ingestion: Option<BundleIngestionSummary>,
    pub warnings: Vec<String>,
    pub quick_links: Vec<ContextBundleQuickLink>,
    pub usage: BundleUsageStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<BundleDiagnostics>,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundleFileMetadata {
    pub path: String,
    pub size: i64,
    pub modified: i64,
    pub hash: String,
    pub last_indexed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    #[serde(skip_serializing)]
    pub content: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct BundleUsageStats {
    pub definitions_tokens: usize,
    pub snippet_tokens: usize,
    pub used_tokens: usize,
    pub budget_tokens: usize,
    pub remaining_tokens: usize,
    pub omitted_snippets: usize,
    pub excerpt_snippets: usize,
    pub summary_snippets: usize,
    pub cache_hit: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleDefinition {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub range_start: Option<i64>,
    pub range_end: Option<i64>,
    pub metadata: Option<Value>,
    pub visibility: Option<String>,
    pub docstring: Option<String>,
    pub todo_count: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundleEdgeNeighbor {
    pub id: String,
    pub r#type: String,
    pub direction: NeighborDirection,
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    pub neighbor: NeighborNode,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NeighborNode {
    pub id: String,
    pub path: Option<String>,
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub range_start: Option<i64>,
    pub range_end: Option<i64>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundleSnippet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub source: SnippetSource,
    pub chunk_index: Option<i32>,
    pub content: String,
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_count: Option<i64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(skip_serializing)]
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Serialize, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct BundleDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity_min: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity_max: Option<f32>,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundleIngestionSummary {
    pub id: String,
    pub finished_at: i64,
    pub duration_ms: i64,
    pub file_count: i64,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContextBundleQuickLink {
    pub r#type: QuickLinkType,
    pub label: String,
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<NeighborDirection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub enum QuickLinkType {
    File,
    RelatedSymbol,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub enum SnippetSource {
    Chunk,
    Content,
}

#[derive(Debug, Serialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum NeighborDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Error)]
pub enum ContextBundleError {
    #[error("failed to resolve workspace root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("blocking task panicked: {0}")]
    Join(#[from] JoinError),
}

pub async fn context_bundle(
    params: ContextBundleParams,
) -> Result<ContextBundleResponse, ContextBundleError> {
    tokio::task::spawn_blocking(move || build_bundle(params)).await?
}

fn build_bundle(params: ContextBundleParams) -> Result<ContextBundleResponse, ContextBundleError> {
    let ContextBundleParams {
        root,
        database_name,
        file,
        symbol,
        max_snippets,
        max_neighbors,
        budget_tokens,
        ranges,
        focus_line,
        query,
    } = params;

    let root_path = resolve_root(root.unwrap_or_else(|| "./".to_string()))?;
    let db_path = root_path.join(database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string()));
    let db_path_string = db_path.to_string_lossy().to_string();

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(ContextBundleError::Sqlite)?;

    let target_file = normalize_file(&file);
    let query_clean = query
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let max_snippets = max_snippets
        .map(|value| value.min(MAX_SNIPPET_LIMIT as u32) as usize)
        .unwrap_or(DEFAULT_SNIPPET_LIMIT);
    let max_neighbors = max_neighbors
        .map(|value| value.min(MAX_NEIGHBOR_LIMIT as u32) as usize)
        .unwrap_or(DEFAULT_NEIGHBOR_LIMIT);
    let budget_tokens = budget_tokens
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_TOKEN_BUDGET);

    let mut file_record =
        load_file_metadata(&conn, &target_file)?.ok_or_else(|| ContextBundleError::Io {
            path: target_file.clone(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "file not indexed"),
        })?;

    let symbol_fingerprint = symbol
        .as_ref()
        .map(|selector| (selector.name.clone(), selector.kind.clone()));

    let mut requested_ranges = ranges.unwrap_or_default();
    requested_ranges.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(a.end_line.cmp(&b.end_line))
    });
    let normalized_ranges: Vec<(u32, u32)> = requested_ranges
        .iter()
        .map(|range| (range.start_line, range.end_line))
        .collect();

    let cache_key = BundleCacheKey {
        database_path: db_path_string.clone(),
        file_path: target_file.clone(),
        file_hash: file_record.hash.clone(),
        symbol: symbol_fingerprint.clone(),
        ranges: normalized_ranges,
        focus_line,
        max_snippets,
        budget_tokens,
        max_neighbors,
        query_fingerprint: query_clean.clone(),
    };

    if let Ok(mut cache) = CONTEXT_BUNDLE_CACHE.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            return Ok(cached);
        }
    }

    let file_content = file_record
        .content
        .clone()
        .or_else(|| read_file_from_disk(&root_path, &target_file).ok());

    let definitions = load_definitions(&conn, &target_file, file_content.as_deref());
    let focus_definition =
        symbol.and_then(|selector| find_focus_definition(&definitions, selector));

    let related = load_related_neighbors(
        &conn,
        &definitions,
        max_neighbors,
        focus_definition.as_ref(),
    );

    let content_ref = file_content.as_deref();
    let line_offsets = content_ref.map(compute_line_offsets);

    let mut embedding_backend = load_meta_value_bundle(&conn, "embedding_backend");
    let mut embedding_model_name: Option<String> = None;
    let mut embedding_latency_ms: Option<u128> = None;
    let mut query_embedding: Option<Vec<f32>> = None;

    if let Some(query_text) = query_clean.clone() {
        if let Ok(models) = available_embedding_models_bundle(&conn) {
            if let Some(model_name) = models.first().cloned() {
                let backend_label = embedding_backend
                    .clone()
                    .unwrap_or_else(|| "onnx".to_string());
                if let Ok(handle) = create_embedding_runner(&model_name, &backend_label) {
                    if let Ok(mut runner) = handle.lock() {
                        let timer = Instant::now();
                        if let Ok(vector) = runner.embed_query(&query_text) {
                            query_embedding = Some(vector);
                            embedding_latency_ms = Some(timer.elapsed().as_millis());
                            embedding_model_name = Some(model_name);
                            if embedding_backend.is_none() {
                                embedding_backend = Some(backend_label);
                            }
                        }
                    }
                }
            }
        }
    }

    let query_vector_ref = query_embedding.as_deref();

    let snippet_request = CollectSnippetsRequest {
        path: &target_file,
        max_snippets,
        ranges: &requested_ranges,
        focus_line,
        file_content: content_ref,
        line_offsets: line_offsets.as_deref(),
        query_embedding: query_vector_ref,
        query_model: embedding_model_name.as_deref(),
    };

    let (mut snippets, mut snippet_warnings) = collect_snippets(&conn, snippet_request);

    let mut existing_keys: HashSet<String> = snippets
        .iter()
        .map(|snippet| snippet_key(snippet, &target_file))
        .collect();

    let (neighbor_snippets, mut neighbor_warnings) = collect_neighbor_snippets(
        &conn,
        &target_file,
        &related,
        query_vector_ref,
        embedding_model_name.as_deref(),
    );

    for snippet in neighbor_snippets {
        let key = snippet_key(&snippet, &target_file);
        if existing_keys.insert(key) {
            snippets.push(snippet);
        }
    }

    snippet_warnings.append(&mut neighbor_warnings);

    let (trimmed_snippets, usage_stats, mut trimming_warnings) =
        trim_snippets_to_budget(snippets, &definitions, budget_tokens);

    let ingestion = load_latest_ingestion(&conn)?;
    let mut warnings = gather_warnings(&definitions, content_ref);
    warnings.append(&mut snippet_warnings);
    warnings.append(&mut trimming_warnings);
    if symbol_fingerprint.is_none() && requested_ranges.is_empty() && focus_line.is_none() {
        warnings.push(
            "No symbol, ranges, or focusLine provided; prefer targeting definitions to minimize context.".to_string(),
        );
    }
    let quick_links = build_quick_links(
        &target_file,
        &definitions,
        &related,
        focus_definition.as_ref(),
    );

    let brief = file_content.as_deref().and_then(build_file_brief);

    let mut diagnostics = BundleDiagnostics {
        query: query_clean.clone(),
        embedding_model: embedding_model_name.clone(),
        embedding_backend,
        embedding_latency_ms,
        similarity_min: None,
        similarity_max: None,
    };

    if let Some(first) = trimmed_snippets
        .iter()
        .filter_map(|snippet| snippet.similarity)
        .next()
    {
        let mut min_sim = first;
        let mut max_sim = first;
        for value in trimmed_snippets
            .iter()
            .filter_map(|snippet| snippet.similarity)
        {
            if value < min_sim {
                min_sim = value;
            }
            if value > max_sim {
                max_sim = value;
            }
        }
        diagnostics.similarity_min = Some(min_sim);
        diagnostics.similarity_max = Some(max_sim);
    }

    let diagnostics_opt = if diagnostics.query.is_some()
        || diagnostics.embedding_model.is_some()
        || diagnostics.embedding_latency_ms.is_some()
        || diagnostics.similarity_min.is_some()
        || diagnostics.similarity_max.is_some()
    {
        Some(diagnostics.clone())
    } else {
        None
    };

    let response = ContextBundleResponse {
        database_path: db_path_string,
        file: BundleFileMetadata {
            path: target_file,
            size: file_record.size,
            modified: file_record.modified,
            hash: file_record.hash,
            last_indexed_at: file_record.last_indexed_at,
            brief,
            content: file_record.content.take(),
        },
        definitions,
        focus_definition,
        related,
        snippets: trimmed_snippets,
        latest_ingestion: ingestion,
        warnings,
        quick_links,
        usage: usage_stats,
        diagnostics: diagnostics_opt.clone(),
    };

    if let Ok(mut cache) = CONTEXT_BUNDLE_CACHE.lock() {
        cache.put(cache_key, response.clone());
    }

    Ok(response)
}

fn resolve_root(root: String) -> Result<PathBuf, ContextBundleError> {
    let candidate = PathBuf::from(root);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let cwd = std::env::current_dir().map_err(|source| ContextBundleError::InvalidRoot {
        path: "./".to_string(),
        source,
    })?;
    Ok(cwd.join(candidate))
}

fn load_file_metadata(
    conn: &Connection,
    path: &str,
) -> Result<Option<BundleFileMetadata>, ContextBundleError> {
    let mut stmt = conn.prepare(
        "SELECT path, size, modified, hash, last_indexed_at, content FROM files WHERE path = ?1",
    )?;

    let record = stmt.query_row(params![path], |row| {
        Ok(BundleFileMetadata {
            path: row.get(0)?,
            size: row.get(1)?,
            modified: row.get(2)?,
            hash: row.get(3)?,
            last_indexed_at: row.get(4)?,
            brief: None,
            content: row.get(5)?,
        })
    });

    match record {
        Ok(file) => Ok(Some(file)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(ContextBundleError::Sqlite(error)),
    }
}

fn read_file_from_disk(root: &Path, relative: &str) -> Result<String, std::io::Error> {
    fs::read_to_string(root.join(relative))
}

fn normalize_file(file: &str) -> String {
    file.replace("\\", "/")
}

fn available_embedding_models_bundle(conn: &Connection) -> Result<Vec<String>, ContextBundleError> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT embedding_model FROM file_chunks")
        .map_err(ContextBundleError::Sqlite)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(ContextBundleError::Sqlite)?;
    Ok(rows.flatten().collect())
}

fn load_meta_value_bundle(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn blob_to_vec(blob: &[u8]) -> Vec<f32> {
    if !blob.len().is_multiple_of(4) {
        return Vec::new();
    }
    let mut values = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        values.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    values
}

fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn load_definitions(conn: &Connection, path: &str, content: Option<&str>) -> Vec<BundleDefinition> {
    let mut stmt = match conn.prepare(
        "SELECT id, name, kind, signature, range_start, range_end, metadata FROM code_graph_nodes WHERE path = ?1 ORDER BY range_start ASC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    let rows = stmt
        .query_map(params![path], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .ok();

    let mut definitions = Vec::new();
    if let Some(rows) = rows {
        for row in rows.flatten() {
            let (id, name, kind, signature, range_start, range_end, metadata_raw) = row;
            let metadata_value = metadata_raw
                .as_deref()
                .and_then(|payload| serde_json::from_str::<Value>(payload).ok());

            let (visibility, docstring, todo_count) = match content {
                Some(text) => (
                    determine_visibility(text, range_start, &kind, metadata_value.as_ref()),
                    extract_docstring(text, range_start),
                    count_todos(text, range_start, range_end),
                ),
                None => (None, None, None),
            };

            definitions.push(BundleDefinition {
                id,
                name,
                kind,
                signature,
                range_start,
                range_end,
                metadata: metadata_value,
                visibility,
                docstring,
                todo_count,
            });
        }
    }

    definitions
}

fn determine_visibility(
    content: &str,
    range_start: Option<i64>,
    kind: &str,
    metadata: Option<&Value>,
) -> Option<String> {
    let start = range_start? as usize;
    if start == 0 || start > content.len() {
        return None;
    }

    let preceding = &content[..start];
    let last_line_start = preceding.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line = content[last_line_start..]
        .lines()
        .next()
        .unwrap_or("")
        .trim();

    if line.contains("private") {
        return Some("private".to_string());
    }
    if line.contains("protected") {
        return Some("protected".to_string());
    }
    if line.contains("public") || line.contains("export") {
        return Some("public".to_string());
    }
    if kind == "method" {
        if let Some(Value::Object(map)) = metadata {
            if map.contains_key("className") {
                return Some("public".to_string());
            }
        }
    }
    Some("internal".to_string())
}

fn extract_docstring(content: &str, range_start: Option<i64>) -> Option<String> {
    let start = range_start? as usize;
    if start == 0 || start > content.len() {
        return None;
    }

    static BLOCK_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?s)/\*\*\s*(.*?)\s*\*/\s*$").unwrap());
    static LINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)(?:(?:\s*//.*\n)+)\s*$").unwrap());

    let prefix = &content[..start];
    if let Some(caps) = BLOCK_RE.captures(prefix) {
        let cleaned = caps
            .get(1)
            .map(|m| m.as_str())
            .unwrap_or("")
            .lines()
            .map(|line| line.trim_start_matches('*').trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }

    if let Some(caps) = LINE_RE.captures(prefix) {
        let cleaned = caps
            .get(0)
            .map(|m| m.as_str())
            .unwrap_or("")
            .lines()
            .map(|line| line.trim_start_matches('/').trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

fn count_todos(content: &str, start: Option<i64>, end: Option<i64>) -> Option<u32> {
    let start = start? as usize;
    let end = end? as usize;
    if start >= end || end > content.len() {
        return None;
    }
    static TODO_RE: Lazy<Regex> = Lazy::new(|| Regex::new("(?i)(TODO|FIXME)").unwrap());
    let snippet = &content[start..end];
    Some(TODO_RE.find_iter(snippet).count() as u32)
}

fn load_related_neighbors(
    conn: &Connection,
    definitions: &[BundleDefinition],
    limit: usize,
    _focus: Option<&BundleDefinition>,
) -> Vec<BundleEdgeNeighbor> {
    if definitions.is_empty() {
        return Vec::new();
    }

    let mut neighbors = Vec::new();
    let mut stmt = match conn.prepare(
        "SELECT id, type, source_id, target_id, source_path, target_path, metadata \
         FROM code_graph_edges \
         WHERE source_id = ?1 OR target_id = ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return neighbors,
    };

    for definition in definitions {
        if neighbors.len() >= limit {
            break;
        }

        let rows = stmt
            .query_map(params![&definition.id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .ok();

        if let Some(rows) = rows {
            for row in rows.flatten() {
                let (
                    edge_id,
                    edge_type,
                    source_id,
                    target_id,
                    source_path,
                    target_path,
                    metadata_raw,
                ) = row;
                let direction = if source_id == definition.id {
                    NeighborDirection::Outgoing
                } else {
                    NeighborDirection::Incoming
                };

                let neighbor_id = if direction == NeighborDirection::Outgoing {
                    &target_id
                } else {
                    &source_id
                };

                if let Some(node) = load_neighbor_node(conn, neighbor_id) {
                    let metadata = metadata_raw
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<Value>(payload).ok());
                    neighbors.push(BundleEdgeNeighbor {
                        id: edge_id,
                        r#type: edge_type,
                        direction,
                        metadata,
                        source_path,
                        target_path,
                        neighbor: node,
                    });
                }

                if neighbors.len() >= limit {
                    break;
                }
            }
        }
    }

    neighbors
}

fn load_neighbor_node(conn: &Connection, node_id: &str) -> Option<NeighborNode> {
    let mut stmt = conn
        .prepare(
            "SELECT id, path, kind, name, signature, range_start, range_end, metadata \
             FROM code_graph_nodes WHERE id = ?1",
        )
        .ok()?;
    stmt.query_row(params![node_id], |row| {
        let metadata_raw: Option<String> = row.get(7)?;
        Ok(NeighborNode {
            id: row.get(0)?,
            path: row.get(1)?,
            kind: row.get(2)?,
            name: row.get(3)?,
            signature: row.get(4)?,
            range_start: row.get(5)?,
            range_end: row.get(6)?,
            metadata: metadata_raw
                .as_deref()
                .and_then(|payload| serde_json::from_str::<Value>(payload).ok()),
        })
    })
    .ok()
}

fn neighbor_file_path(neighbor: &BundleEdgeNeighbor) -> Option<&str> {
    neighbor
        .neighbor
        .path
        .as_deref()
        .or(match neighbor.direction {
            NeighborDirection::Outgoing => neighbor.target_path.as_deref(),
            NeighborDirection::Incoming => neighbor.source_path.as_deref(),
        })
}

fn collect_neighbor_snippets(
    conn: &Connection,
    primary_path: &str,
    neighbors: &[BundleEdgeNeighbor],
    query_embedding: Option<&[f32]>,
    query_model: Option<&str>,
) -> (Vec<BundleSnippet>, Vec<String>) {
    let mut snippets = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();

    for neighbor in neighbors {
        match load_neighbor_snippet(conn, neighbor) {
            Ok(Some(snippet)) => {
                let key = snippet_key(&snippet, primary_path);
                if seen_keys.insert(key) {
                    snippets.push(decorate_neighbor_snippet(
                        snippet,
                        neighbor,
                        query_embedding,
                        query_model,
                    ));
                }
            }
            Ok(None) => {}
            Err(message) => warnings.push(message),
        }
    }

    (snippets, warnings)
}

fn load_neighbor_snippet(
    conn: &Connection,
    neighbor: &BundleEdgeNeighbor,
) -> Result<Option<BundleSnippet>, String> {
    let path = match neighbor_file_path(neighbor) {
        Some(path) => path,
        None => {
            return Err(format!(
                "Skipping neighbor '{}' because no file path was recorded.",
                neighbor.neighbor.name
            ))
        }
    };

    if let Some(range_start) = neighbor.neighbor.range_start {
        let range_end = neighbor.neighbor.range_end.unwrap_or(range_start);
        let mut stmt = conn
            .prepare(
                "SELECT chunk_index, content, summary, symbol, identifier, source_type, metadata, embedding_model, embedding, byte_start, byte_end, line_start, line_end, hits \
                 FROM file_chunks \
                 WHERE path = ?1 AND byte_start IS NOT NULL AND byte_end IS NOT NULL AND byte_start <= ?2 AND byte_end >= ?3 \
                 ORDER BY hits ASC \
                 LIMIT 1",
            )
            .map_err(|error| error.to_string())?;

        match stmt.query_row(params![path, range_start, range_end], |row| {
            snippet_from_row(row, true, path)
        }) {
            Ok(snippet) => return Ok(Some(snippet)),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(error.to_string()),
        }
    }

    let mut identifier_attempts: HashSet<&str> = HashSet::new();
    for candidate in [
        extract_identifier(&neighbor.neighbor.metadata),
        Some(neighbor.neighbor.id.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if !identifier_attempts.insert(candidate) {
            continue;
        }

        let mut stmt = conn
            .prepare(
                "SELECT chunk_index, content, summary, symbol, identifier, source_type, metadata, embedding_model, embedding, byte_start, byte_end, line_start, line_end, hits \
                 FROM file_chunks \
                 WHERE path = ?1 AND identifier = ?2 \
                 ORDER BY hits ASC \
                 LIMIT 1",
            )
            .map_err(|error| error.to_string())?;

        match stmt.query_row(params![path, candidate], |row| {
            snippet_from_row(row, true, path)
        }) {
            Ok(snippet) => return Ok(Some(snippet)),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(error.to_string()),
        }
    }

    let mut stmt = conn
        .prepare(
            "SELECT chunk_index, content, summary, symbol, identifier, source_type, metadata, embedding_model, embedding, byte_start, byte_end, line_start, line_end, hits \
             FROM file_chunks \
             WHERE path = ?1 AND symbol = ?2 \
             ORDER BY hits ASC \
             LIMIT 1",
        )
        .map_err(|error| error.to_string())?;

    match stmt.query_row(params![path, neighbor.neighbor.name.as_str()], |row| {
        snippet_from_row(row, true, path)
    }) {
        Ok(snippet) => return Ok(Some(snippet)),
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(error) => return Err(error.to_string()),
    }

    let mut fallback = load_snippets(conn, path, 1, true);
    Ok(fallback.pop())
}

fn decorate_neighbor_snippet(
    mut snippet: BundleSnippet,
    neighbor: &BundleEdgeNeighbor,
    query_embedding: Option<&[f32]>,
    query_model: Option<&str>,
) -> BundleSnippet {
    if snippet.path.is_none() {
        snippet.path = neighbor_file_path(neighbor).map(|path| path.to_string());
    }

    let mut score = snippet.score.unwrap_or(0.0) + 48.0;
    if let (Some(query_vec), Some(embed_vec), Some(model_name)) = (
        query_embedding,
        snippet.embedding.as_ref(),
        snippet.embedding_model.as_deref(),
    ) {
        if query_model
            .map(|expected| expected == model_name)
            .unwrap_or(true)
        {
            let similarity = dot_product(query_vec, embed_vec);
            snippet.similarity = Some(similarity);
            score += similarity * 12.0;
        }
    }
    snippet.score = Some(score);
    snippet.metadata = merge_neighbor_metadata(snippet.metadata.take(), neighbor);
    snippet
}

fn merge_neighbor_metadata(
    existing: Option<Value>,
    neighbor: &BundleEdgeNeighbor,
) -> Option<Value> {
    let mut map = match existing {
        Some(Value::Object(map)) => map,
        Some(other) => {
            let mut wrapper = serde_json::Map::new();
            wrapper.insert("snippet".to_string(), other);
            wrapper
        }
        None => serde_json::Map::new(),
    };

    map.entry("neighborEdgeId".to_string())
        .or_insert(Value::String(neighbor.id.clone()));
    map.entry("neighborType".to_string())
        .or_insert(Value::String(neighbor.r#type.clone()));
    map.entry("neighborDirection".to_string())
        .or_insert(Value::String(
            match neighbor.direction {
                NeighborDirection::Incoming => "incoming",
                NeighborDirection::Outgoing => "outgoing",
            }
            .to_string(),
        ));
    if let Some(path) = neighbor_file_path(neighbor) {
        map.entry("neighborPath".to_string())
            .or_insert(Value::String(path.to_string()));
    }
    if let Some(source_path) = neighbor.source_path.as_deref() {
        map.entry("neighborSourcePath".to_string())
            .or_insert(Value::String(source_path.to_string()));
    }
    if let Some(target_path) = neighbor.target_path.as_deref() {
        map.entry("neighborTargetPath".to_string())
            .or_insert(Value::String(target_path.to_string()));
    }
    map.entry("neighborSymbolId".to_string())
        .or_insert(Value::String(neighbor.neighbor.id.clone()));
    map.entry("neighborSymbolName".to_string())
        .or_insert(Value::String(neighbor.neighbor.name.clone()));
    map.entry("neighborSymbolKind".to_string())
        .or_insert(Value::String(neighbor.neighbor.kind.clone()));

    Some(Value::Object(map))
}

fn extract_identifier(metadata: &Option<Value>) -> Option<&str> {
    metadata
        .as_ref()
        .and_then(|value| value.get("identifier"))
        .and_then(Value::as_str)
}

fn load_snippets(
    conn: &Connection,
    path: &str,
    max_snippets: usize,
    include_path: bool,
) -> Vec<BundleSnippet> {
    let mut stmt = match conn.prepare(
        "SELECT chunk_index, content, summary, symbol, identifier, source_type, metadata, embedding_model, embedding, byte_start, byte_end, line_start, line_end, hits \
         FROM file_chunks \
         WHERE path = ?1 \
         ORDER BY hits ASC, chunk_index ASC \
         LIMIT ?2",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(params![path, max_snippets as i64], |row| {
        snippet_from_row(row, include_path, path)
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

fn snippet_from_row(
    row: &rusqlite::Row<'_>,
    include_path: bool,
    path: &str,
) -> rusqlite::Result<BundleSnippet> {
    let metadata_raw: Option<String> = row.get(6)?;
    let embedding_blob: Vec<u8> = row.get(8)?;
    let embedding = blob_to_vec(&embedding_blob);
    Ok(BundleSnippet {
        path: include_path.then(|| path.to_string()),
        source: SnippetSource::Chunk,
        chunk_index: Some(row.get(0)?),
        content: row.get(1)?,
        byte_start: row.get(9)?,
        byte_end: row.get(10)?,
        line_start: row.get(11)?,
        line_end: row.get(12)?,
        served_count: row.get(13)?,
        summary: row.get(2)?,
        symbol: row.get(3)?,
        identifier: row.get(4)?,
        source_type: row.get(5)?,
        metadata: metadata_raw
            .as_ref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok()),
        score: None,
        similarity: None,
        embedding_model: row.get(7)?,
        embedding: if embedding.is_empty() {
            None
        } else {
            Some(embedding)
        },
    })
}

struct CollectSnippetsRequest<'a> {
    path: &'a str,
    max_snippets: usize,
    ranges: &'a [LineRange],
    focus_line: Option<u32>,
    file_content: Option<&'a str>,
    line_offsets: Option<&'a [usize]>,
    query_embedding: Option<&'a [f32]>,
    query_model: Option<&'a str>,
}

fn collect_snippets(
    conn: &Connection,
    request: CollectSnippetsRequest<'_>,
) -> (Vec<BundleSnippet>, Vec<String>) {
    let CollectSnippetsRequest {
        path,
        max_snippets,
        ranges,
        focus_line,
        file_content,
        line_offsets,
        query_embedding,
        query_model,
    } = request;
    #[derive(Debug)]
    struct Candidate {
        snippet: BundleSnippet,
        score: f32,
        order: usize,
    }

    let mut warnings = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen = HashSet::new();
    let mut order = 0usize;
    let mut push_candidate = |snippet: BundleSnippet, score: f32| {
        let key = snippet_key(&snippet, path);
        if seen.insert(key) {
            candidates.push(Candidate {
                snippet,
                score,
                order,
            });
            order += 1;
        }
    };

    let mut had_range_request = false;

    if !ranges.is_empty() {
        had_range_request = true;
        if let (Some(content), Some(offsets)) = (file_content, line_offsets) {
            for range in ranges {
                if let Some(snippet) =
                    build_range_snippet(None, content, offsets, range.start_line, range.end_line)
                {
                    let mut snippet = snippet;
                    let mut score = 120.0 + snippet_semantic_weight(&snippet.content);
                    if let Some(line) = focus_line {
                        score += proximity_bonus(&snippet, line);
                    }
                    if let Some(query_vec) = query_embedding {
                        if let Some(embed_vec) = snippet.embedding.as_ref() {
                            let model_matches =
                                match (query_model, snippet.embedding_model.as_deref()) {
                                    (Some(expected), Some(actual)) => expected == actual,
                                    (Some(_), None) => false,
                                    _ => true,
                                };
                            if model_matches {
                                let similarity = dot_product(query_vec, embed_vec);
                                snippet.similarity = Some(similarity);
                                score += similarity * 10.0;
                            }
                        }
                    }
                    score -= snippet_usage_penalty(snippet.served_count);
                    snippet.score = Some(score);
                    push_candidate(snippet, score);
                } else {
                    warnings.push(format!(
                        "Range {}-{} could not be assembled from cached content.",
                        range.start_line, range.end_line
                    ));
                }
            }
        } else {
            warnings
                .push("Requested ranges ignored because file content is unavailable.".to_string());
        }
    }

    if let Some(line) = focus_line {
        if let (Some(content), Some(offsets)) = (file_content, line_offsets) {
            if let Some(snippet) = build_focus_snippet(content, offsets, line) {
                let mut snippet = snippet;
                let mut score = 110.0 + snippet_semantic_weight(&snippet.content);
                if let Some(query_vec) = query_embedding {
                    if let Some(embed_vec) = snippet.embedding.as_ref() {
                        let model_matches = match (query_model, snippet.embedding_model.as_deref())
                        {
                            (Some(expected), Some(actual)) => expected == actual,
                            (Some(_), None) => false,
                            _ => true,
                        };
                        if model_matches {
                            let similarity = dot_product(query_vec, embed_vec);
                            snippet.similarity = Some(similarity);
                            score += similarity * 10.0;
                        }
                    }
                }
                let adjusted = score - snippet_usage_penalty(snippet.served_count);
                snippet.score = Some(adjusted);
                push_candidate(snippet, adjusted);
            }
        } else {
            warnings.push("focusLine ignored because file content is unavailable.".to_string());
        }
    }

    let fetch_limit = std::cmp::max(max_snippets, 1)
        .saturating_mul(3)
        .min(MAX_SNIPPET_LIMIT);
    for mut snippet in load_snippets(conn, path, fetch_limit, false) {
        let mut score = 30.0 + snippet_semantic_weight(&snippet.content);
        if let Some(line) = focus_line {
            score += proximity_bonus(&snippet, line);
        }
        if let Some(query_vec) = query_embedding {
            if let Some(embed_vec) = snippet.embedding.as_ref() {
                let model_matches = match (query_model, snippet.embedding_model.as_deref()) {
                    (Some(expected), Some(actual)) => expected == actual,
                    (Some(_), None) => false,
                    _ => true,
                };
                if model_matches {
                    let similarity = dot_product(query_vec, embed_vec);
                    snippet.similarity = Some(similarity);
                    score += similarity * 10.0;
                }
            }
        }
        let adjusted = score - snippet_usage_penalty(snippet.served_count);
        snippet.score = Some(adjusted);
        push_candidate(snippet, adjusted);
    }

    if candidates.is_empty() {
        let mut fallback = load_snippets(conn, path, max_snippets.max(1), false);
        if fallback.is_empty() {
            warnings.push("No snippets available for the requested file.".to_string());
        } else if had_range_request || focus_line.is_some() {
            warnings.push(
                "No stored snippets matched the requested focus; returning database defaults."
                    .to_string(),
            );
        }

        if let Some(query_vec) = query_embedding {
            for snippet in fallback.iter_mut() {
                if let Some(embed_vec) = snippet.embedding.as_ref() {
                    let model_matches = match (query_model, snippet.embedding_model.as_deref()) {
                        (Some(expected), Some(actual)) => expected == actual,
                        (Some(_), None) => false,
                        _ => true,
                    };
                    if model_matches {
                        let similarity = dot_product(query_vec, embed_vec);
                        snippet.similarity = Some(similarity);
                        snippet.score = Some(similarity);
                    }
                }
            }
        }

        for snippet in fallback.iter_mut() {
            snippet.embedding = None;
        }

        return (fallback, warnings);
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.order.cmp(&b.order))
    });

    let selected: Vec<BundleSnippet> = candidates
        .into_iter()
        .take(max_snippets)
        .map(|candidate| candidate.snippet)
        .collect();

    if selected.is_empty() && max_snippets > 0 {
        warnings.push("No snippets available for the requested file.".to_string());
    }

    (selected, warnings)
}

fn snippet_key(snippet: &BundleSnippet, fallback_path: &str) -> String {
    let path = snippet.path.as_deref().unwrap_or(fallback_path).to_string();
    format!(
        "{}:{:?}:{:?}:{:?}:{:?}:{:?}",
        path,
        snippet.source,
        snippet.chunk_index,
        snippet.line_start,
        snippet.line_end,
        snippet.byte_start
    )
}

fn snippet_semantic_weight(content: &str) -> f32 {
    let lower = content.to_lowercase();
    const STRUCTURAL_MARKERS: [&str; 8] = [
        "fn ",
        "pub ",
        "struct ",
        "class ",
        "impl ",
        "def ",
        "enum ",
        "interface ",
    ];

    let mut score = 0.0;
    if STRUCTURAL_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        score += 8.0;
    }

    if lower.contains("///") || lower.contains("/**") {
        score += 3.0;
    }

    if lower.contains("todo") || lower.contains("fixme") {
        score -= 2.0;
    }

    if lower.len() < 120 {
        score += 2.0;
    }

    score
}

fn snippet_usage_penalty(served: Option<i64>) -> f32 {
    served
        .map(|count| (count.max(0) as f32 + 1.0).ln() * 6.0)
        .unwrap_or(0.0)
}

fn proximity_bonus(snippet: &BundleSnippet, focus_line: u32) -> f32 {
    if snippet_covers_line(snippet, focus_line) {
        return 25.0;
    }

    match snippet_focus_distance(snippet, focus_line) {
        Some(0) => 25.0,
        Some(1..=5) => 15.0,
        Some(6..=15) => 8.0,
        Some(16..=40) => 4.0,
        Some(_) => 1.0,
        None => 0.0,
    }
}

fn snippet_focus_distance(snippet: &BundleSnippet, focus_line: u32) -> Option<u32> {
    let (start, end) = snippet_line_span(snippet)?;
    if focus_line < start {
        start.checked_sub(focus_line)
    } else if focus_line > end {
        focus_line.checked_sub(end)
    } else {
        Some(0)
    }
}

fn snippet_line_span(snippet: &BundleSnippet) -> Option<(u32, u32)> {
    let start = u32::try_from(*snippet.line_start.as_ref()?)
        .ok()
        .filter(|value| *value > 0)?;
    let end = u32::try_from(*snippet.line_end.as_ref()?)
        .ok()
        .filter(|value| *value > 0)?;

    if start <= end {
        Some((start, end))
    } else {
        Some((end, start))
    }
}

fn build_range_snippet(
    path: Option<&str>,
    content: &str,
    offsets: &[usize],
    start_line: u32,
    end_line: u32,
) -> Option<BundleSnippet> {
    if offsets.len() < 2 {
        return None;
    }

    let line_count = offsets.len() - 1;
    if line_count == 0 {
        return None;
    }

    let start = start_line.max(1) as usize;
    if start > line_count {
        return None;
    }

    let mut end = end_line.max(start_line) as usize;
    if end > line_count {
        end = line_count;
    }

    let start_index = start - 1;
    let end_index = end;

    if start_index >= offsets.len() || end_index >= offsets.len() || start_index >= end_index {
        return None;
    }

    let start_byte = offsets[start_index];
    let end_byte = offsets[end_index];
    let snippet_content = content[start_byte..end_byte].to_string();

    Some(BundleSnippet {
        path: path.map(|value| value.to_string()),
        source: SnippetSource::Content,
        chunk_index: None,
        content: snippet_content,
        byte_start: Some(start_byte as i64),
        byte_end: Some(end_byte as i64),
        line_start: Some(start as i64),
        line_end: Some(end as i64),
        served_count: None,
        summary: None,
        symbol: None,
        identifier: None,
        source_type: Some("content".to_string()),
        metadata: None,
        score: None,
        similarity: None,
        embedding_model: None,
        embedding: None,
    })
}

fn build_focus_snippet(content: &str, offsets: &[usize], focus_line: u32) -> Option<BundleSnippet> {
    if offsets.len() < 2 {
        return None;
    }

    let line_count = (offsets.len() - 1) as u32;
    if line_count == 0 {
        return None;
    }

    let clamped = focus_line.clamp(1, line_count);
    let start_line = clamped.saturating_sub(FOCUS_CONTEXT_RADIUS).max(1);
    let mut end_line = clamped + FOCUS_CONTEXT_RADIUS;
    if end_line > line_count {
        end_line = line_count;
    }

    build_range_snippet(None, content, offsets, start_line, end_line)
}

fn snippet_covers_line(snippet: &BundleSnippet, line: u32) -> bool {
    match (snippet.line_start, snippet.line_end) {
        (Some(start), Some(end)) => {
            let line = line as i64;
            line >= start && line <= end
        }
        _ => false,
    }
}

fn compute_line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    offsets.push(0);
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            offsets.push(idx + ch.len_utf8());
        }
    }
    if let Some(last) = offsets.last().copied() {
        if last != content.len() {
            offsets.push(content.len());
        }
    }
    offsets
}

fn trim_snippets_to_budget(
    snippets: Vec<BundleSnippet>,
    definitions: &[BundleDefinition],
    budget_tokens: usize,
) -> (Vec<BundleSnippet>, BundleUsageStats, Vec<String>) {
    #[derive(Copy, Clone, Eq, PartialEq)]
    enum Stage {
        Omitted,
        Summary,
        Excerpt,
        Full,
    }

    struct SnippetEntry {
        path: Option<String>,
        source: SnippetSource,
        chunk_index: Option<i32>,
        byte_start: Option<i64>,
        byte_end: Option<i64>,
        line_start: Option<i64>,
        line_end: Option<i64>,
        served_count: Option<i64>,
        summary: Option<String>,
        symbol: Option<String>,
        identifier: Option<String>,
        source_type: Option<String>,
        metadata: Option<Value>,
        score: Option<f32>,
        similarity: Option<f32>,
        embedding_model: Option<String>,
        summary_content: String,
        summary_tokens: usize,
        excerpt_content: Option<String>,
        excerpt_tokens: Option<usize>,
        full_content: String,
        full_tokens: usize,
        stage: Stage,
    }

    impl SnippetEntry {
        fn new(snippet: BundleSnippet) -> Self {
            let summary_content = build_summary_content(&snippet.content);
            let summary_tokens = estimate_tokens(&summary_content).max(MIN_SUMMARY_TOKEN_FLOOR);
            let excerpt_content = build_excerpt_content(&snippet.content);
            let excerpt_tokens = excerpt_content
                .as_ref()
                .map(|content| estimate_tokens(content));
            let full_tokens = estimate_tokens(&snippet.content);

            SnippetEntry {
                path: snippet.path,
                source: snippet.source,
                chunk_index: snippet.chunk_index,
                byte_start: snippet.byte_start,
                byte_end: snippet.byte_end,
                line_start: snippet.line_start,
                line_end: snippet.line_end,
                served_count: snippet.served_count,
                summary: snippet.summary,
                symbol: snippet.symbol,
                identifier: snippet.identifier,
                source_type: snippet.source_type,
                metadata: snippet.metadata,
                score: snippet.score,
                similarity: snippet.similarity,
                embedding_model: snippet.embedding_model,
                summary_content,
                summary_tokens,
                excerpt_content,
                excerpt_tokens,
                full_content: snippet.content,
                full_tokens,
                stage: Stage::Omitted,
            }
        }

        fn tokens_for_stage(&self, stage: Stage) -> Option<usize> {
            match stage {
                Stage::Omitted => Some(0),
                Stage::Summary => Some(self.summary_tokens),
                Stage::Excerpt => self.excerpt_tokens,
                Stage::Full => Some(self.full_tokens),
            }
        }

        fn upgrade_cost(&self, from: Stage, to: Stage) -> Option<usize> {
            let from_tokens = self.tokens_for_stage(from)?;
            let to_tokens = self.tokens_for_stage(to)?;
            if to_tokens > from_tokens {
                Some(to_tokens - from_tokens)
            } else {
                None
            }
        }

        fn finalize(self) -> Option<BundleSnippet> {
            let SnippetEntry {
                path,
                source,
                chunk_index,
                byte_start,
                byte_end,
                line_start,
                line_end,
                served_count,
                summary,
                symbol,
                identifier,
                source_type,
                metadata,
                score,
                similarity,
                embedding_model,
                summary_content,
                excerpt_content,
                full_content,
                stage,
                ..
            } = self;

            let content = match stage {
                Stage::Omitted => return None,
                Stage::Summary => summary_content,
                Stage::Excerpt => excerpt_content.unwrap_or_else(|| full_content.clone()),
                Stage::Full => full_content,
            };

            Some(BundleSnippet {
                path,
                source,
                chunk_index,
                content,
                byte_start,
                byte_end,
                line_start,
                line_end,
                served_count,
                summary,
                symbol,
                identifier,
                source_type,
                metadata,
                score,
                similarity,
                embedding_model,
                embedding: None,
            })
        }
    }

    let definitions_cost = definition_token_cost(definitions);
    let mut used_tokens = definitions_cost;
    let mut warnings = Vec::new();
    let mut usage = BundleUsageStats {
        definitions_tokens: definitions_cost,
        budget_tokens,
        ..BundleUsageStats::default()
    };

    if definitions_cost > budget_tokens {
        warnings.push(format!(
            "Definition metadata consumes {} tokens which already exceeds the {} token budget; snippet content may be omitted.",
            definitions_cost, budget_tokens
        ));
    }

    if snippets.is_empty() {
        usage.snippet_tokens = 0;
        usage.used_tokens = used_tokens;
        usage.remaining_tokens = budget_tokens.saturating_sub(used_tokens);
        return (Vec::new(), usage, Vec::new());
    }

    let mut entries: Vec<SnippetEntry> = snippets.into_iter().map(SnippetEntry::new).collect();

    for entry in entries.iter_mut() {
        let summary_tokens = entry.summary_tokens;
        if used_tokens + summary_tokens <= budget_tokens && summary_tokens > 0 {
            used_tokens += summary_tokens;
            entry.stage = Stage::Summary;
        } else {
            entry.stage = Stage::Omitted;
        }
    }

    for entry in entries.iter_mut() {
        if entry.stage != Stage::Summary {
            continue;
        }
        if let Some(cost) = entry.upgrade_cost(Stage::Summary, Stage::Excerpt) {
            if used_tokens + cost <= budget_tokens {
                used_tokens += cost;
                entry.stage = Stage::Excerpt;
            }
        }
    }

    for entry in entries.iter_mut() {
        if entry.stage == Stage::Omitted {
            continue;
        }
        if let Some(cost) = entry.upgrade_cost(entry.stage, Stage::Full) {
            if used_tokens + cost <= budget_tokens {
                used_tokens += cost;
                entry.stage = Stage::Full;
            }
        }
    }

    let mut selected = Vec::new();
    let mut summary_count = 0usize;
    let mut excerpt_count = 0usize;
    let mut omitted_count = 0usize;

    for entry in entries.into_iter() {
        match entry.stage {
            Stage::Summary => summary_count += 1,
            Stage::Excerpt => excerpt_count += 1,
            Stage::Omitted => omitted_count += 1,
            Stage::Full => {}
        }

        if let Some(snippet) = entry.finalize() {
            selected.push(snippet);
        }
    }

    usage.summary_snippets = summary_count;
    usage.excerpt_snippets = excerpt_count;
    usage.omitted_snippets = omitted_count;

    if omitted_count > 0 {
        warnings.push(format!(
            "{} snippet(s) omitted due to the {} token budget; request additional budgetTokens for more detail.",
            omitted_count, budget_tokens
        ));
    }
    if excerpt_count > 0 {
        warnings.push(format!(
            "{} snippet(s) returned as focused excerpts because the full content would exceed the token budget.",
            excerpt_count
        ));
    }
    if summary_count > 0 {
        warnings.push(format!(
            "{} snippet(s) returned as summaries; increase budgetTokens or narrow ranges for full context.",
            summary_count
        ));
    }

    if budget_tokens > 0 {
        let snippet_tokens_used = used_tokens.saturating_sub(definitions_cost);
        usage.snippet_tokens = snippet_tokens_used;
        usage.used_tokens = used_tokens;
        usage.remaining_tokens = budget_tokens.saturating_sub(used_tokens);
        warnings.push(format!(
            "Token usage: definitions {} + snippets {} = {} of {} ({} unused).",
            definitions_cost,
            snippet_tokens_used,
            used_tokens,
            budget_tokens,
            budget_tokens.saturating_sub(used_tokens)
        ));
    } else {
        usage.used_tokens = used_tokens;
    }

    (selected, usage, warnings)
}

fn definition_token_cost(definitions: &[BundleDefinition]) -> usize {
    let mut total = 0usize;
    for definition in definitions {
        total += estimate_tokens(&definition.name);
        if let Some(signature) = &definition.signature {
            total += estimate_tokens(signature);
        }
        if let Some(docstring) = &definition.docstring {
            total += estimate_tokens(docstring);
        }
    }
    total
}

fn build_file_brief(content: &str) -> Option<String> {
    let snippet = content
        .lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())?;
    let brief = truncate_to_char_limit(snippet, SUMMARY_CHAR_LIMIT);
    Some(brief)
}

fn build_summary_content(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return "Summary: (empty snippet)".to_string();
    }

    let first_line = trimmed
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(trimmed)
        .trim();

    let mut summary = truncate_to_char_limit(first_line, SUMMARY_CHAR_LIMIT);
    if summary.len() < trimmed.len() {
        summary.push_str(" …");
    }

    format!("Summary: {summary}")
}

fn build_excerpt_content(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let char_limit = EXCERPT_TOKEN_LIMIT.saturating_mul(4);
    let total_chars = trimmed.chars().count();
    if total_chars <= char_limit {
        return None;
    }

    let mut excerpt = truncate_to_char_limit(trimmed, char_limit);
    if !excerpt.ends_with('\n') {
        excerpt.push('\n');
    }
    excerpt.push_str("... (excerpt truncated)");
    Some(excerpt)
}

fn truncate_to_char_limit(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }

    let mut buffer = String::with_capacity(limit);
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            break;
        }
        buffer.push(ch);
    }
    buffer
}

fn estimate_tokens(text: &str) -> usize {
    ((text.len() as f64 / 4.0).ceil()) as usize
}

fn load_latest_ingestion(
    conn: &Connection,
) -> Result<Option<BundleIngestionSummary>, ContextBundleError> {
    let mut stmt = conn.prepare(
        "SELECT id, finished_at, file_count, finished_at - started_at AS duration FROM ingestions ORDER BY finished_at DESC LIMIT 1",
    )?;

    match stmt.query_row([], |row| {
        Ok(BundleIngestionSummary {
            id: row.get(0)?,
            finished_at: row.get(1)?,
            file_count: row.get(2)?,
            duration_ms: row.get(3)?,
        })
    }) {
        Ok(summary) => Ok(Some(summary)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(ContextBundleError::Sqlite(error)),
    }
}

fn gather_warnings(definitions: &[BundleDefinition], content: Option<&str>) -> Vec<String> {
    let mut warnings = Vec::new();
    if definitions.is_empty() {
        warnings.push("No graph metadata recorded for the requested file.".to_string());
    }
    if content.is_none() {
        warnings
            .push("File content was not stored in the index; snippets may be limited.".to_string());
    }
    warnings
}

fn build_quick_links(
    path: &str,
    definitions: &[BundleDefinition],
    neighbors: &[BundleEdgeNeighbor],
    focus: Option<&BundleDefinition>,
) -> Vec<ContextBundleQuickLink> {
    let mut links = Vec::new();
    links.push(ContextBundleQuickLink {
        r#type: QuickLinkType::File,
        label: path.to_string(),
        path: Some(path.to_string()),
        direction: None,
        symbol_id: None,
        symbol_kind: None,
    });

    if let Some(definition) = focus {
        links.push(ContextBundleQuickLink {
            r#type: QuickLinkType::RelatedSymbol,
            label: definition.name.clone(),
            path: Some(path.to_string()),
            direction: None,
            symbol_id: Some(definition.id.clone()),
            symbol_kind: Some(definition.kind.clone()),
        });
    }

    for definition in definitions {
        if focus.map(|f| f.id.as_str()) == Some(definition.id.as_str()) {
            continue;
        }
        links.push(ContextBundleQuickLink {
            r#type: QuickLinkType::RelatedSymbol,
            label: definition.name.clone(),
            path: Some(path.to_string()),
            direction: None,
            symbol_id: Some(definition.id.clone()),
            symbol_kind: Some(definition.kind.clone()),
        });
    }

    for neighbor in neighbors {
        links.push(ContextBundleQuickLink {
            r#type: QuickLinkType::RelatedSymbol,
            label: neighbor.neighbor.name.clone(),
            path: neighbor_file_path(neighbor).map(|path| path.to_string()),
            direction: Some(neighbor.direction),
            symbol_id: Some(neighbor.neighbor.id.clone()),
            symbol_kind: Some(neighbor.neighbor.kind.clone()),
        });
    }

    links.truncate(16);
    links
}

fn find_focus_definition(
    definitions: &[BundleDefinition],
    selector: SymbolSelector,
) -> Option<BundleDefinition> {
    let SymbolSelector { name, kind } = selector;
    let name_lower = name.to_lowercase();
    definitions
        .iter()
        .find(|definition| {
            definition.name.to_lowercase() == name_lower
                && kind
                    .as_ref()
                    .map(|wanted| definition.kind.eq_ignore_ascii_case(wanted))
                    .unwrap_or(true)
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_snippet(content: &str) -> BundleSnippet {
        BundleSnippet {
            path: None,
            source: SnippetSource::Content,
            chunk_index: Some(0),
            content: content.to_string(),
            byte_start: Some(0),
            byte_end: Some(content.len() as i64),
            line_start: Some(1),
            line_end: Some(content.lines().count() as i64),
            served_count: None,
            summary: None,
            symbol: None,
            identifier: None,
            source_type: None,
            metadata: None,
            score: None,
            similarity: None,
            embedding_model: None,
            embedding: None,
        }
    }

    #[test]
    fn trims_to_summary_when_budget_is_tight() {
        let long_content = "fn example() {\n    println!(\"hello world\");\n}\n".repeat(40);
        let snippets = vec![build_snippet(&long_content)];

        let (result, usage, warnings) =
            trim_snippets_to_budget(snippets, &[], /* budget_tokens */ 60);

        assert_eq!(result.len(), 1);
        let content = &result[0].content;
        assert!(content.starts_with("Summary:"), "content: {content}");
        assert!(warnings.iter().any(|warning| warning.contains("summaries")));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("Token usage")));
        assert_eq!(usage.summary_snippets, 1);
        assert!(usage.snippet_tokens > 0);
    }

    #[test]
    fn upgrades_to_excerpt_when_budget_allows() {
        let long_content = (0..500)
            .map(|idx| format!("println!(\"line {idx}\");"))
            .collect::<Vec<_>>()
            .join("\n");
        let snippets = vec![build_snippet(&long_content)];

        let (result, usage, warnings) =
            trim_snippets_to_budget(snippets, &[], /* budget_tokens */ 360);

        assert_eq!(result.len(), 1);
        let content = &result[0].content;
        assert!(
            content.contains("... (excerpt truncated)"),
            "content: {content}"
        );
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("focused excerpts")));
        assert_eq!(usage.excerpt_snippets, 1);
        assert!(usage.snippet_tokens > 0);
    }
}
