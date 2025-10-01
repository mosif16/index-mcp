use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags, Transaction};
use serde::{Deserialize, Serialize};
use serde_json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    graph::{extract_graph, GraphExtraction},
    index_status::DEFAULT_DB_FILENAME,
};

pub(crate) const DEFAULT_INCLUDE_GLOBS: &[&str] = &["**/*"];
pub(crate) const DEFAULT_EXCLUDE_GLOBS: &[&str] = &[
    "**/.git/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/.mcp-index.sqlite",
    "**/node_modules/**",
    "**/vendor/**",
    "**/dist/**",
    "**/build/**",
];

pub(crate) const DEFAULT_EMBEDDING_MODEL: &str = "Xenova/bge-small-en-v1.5";
const DEFAULT_CHUNK_SIZE_TOKENS: usize = 256;
const DEFAULT_CHUNK_OVERLAP_TOKENS: usize = 32;
const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 32;
const DEFAULT_MAX_DATABASE_SIZE_BYTES: u64 = 150 * 1024 * 1024; // 150 MB

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IngestParams {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    #[serde(default)]
    pub database_name: Option<String>,
    #[serde(default)]
    pub max_file_size_bytes: Option<f64>,
    #[serde(default)]
    pub store_file_content: Option<bool>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub auto_evict: Option<bool>,
    #[serde(default)]
    pub max_database_size_bytes: Option<f64>,
    #[serde(default)]
    pub embedding: Option<EmbeddingParams>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingParams {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub chunk_size_tokens: Option<u32>,
    #[serde(default)]
    pub chunk_overlap_tokens: Option<u32>,
    #[serde(default)]
    pub batch_size: Option<u32>,
}

struct EmbeddingConfig {
    enabled: bool,
    model: String,
    chunk_size_tokens: usize,
    chunk_overlap_tokens: usize,
    batch_size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    pub root: String,
    pub database_path: String,
    pub database_size_bytes: u64,
    pub ingested_file_count: usize,
    pub skipped: Vec<SkippedFile>,
    pub deleted_paths: Vec<String>,
    pub duration_ms: u128,
    pub embedded_chunk_count: usize,
    pub embedding_model: Option<String>,
    pub graph_node_count: usize,
    pub graph_edge_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evicted: Option<EvictionReport>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvictionReport {
    pub database_path: String,
    pub size_before: u64,
    pub size_after: u64,
    pub evicted_chunks: usize,
    pub evicted_nodes: usize,
}

#[derive(Debug)]
struct ScannedFile {
    path: String,
    size: u64,
    modified_ms: i64,
    hash: String,
    stored_content: Option<String>,
    text_content: Option<String>,
}

#[derive(Debug)]
struct ScanOutcome {
    files: Vec<ScannedFile>,
    skipped: Vec<SkippedFile>,
}

#[derive(Debug)]
struct ChunkFragment {
    content: String,
    byte_start: u32,
    byte_end: u32,
    line_start: u32,
    line_end: u32,
}

#[derive(Debug)]
struct ChunkRecord {
    id: String,
    path: String,
    chunk_index: i32,
    content: String,
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    line_start: Option<i64>,
    line_end: Option<i64>,
    embedding: Option<Vec<f32>>,
}

#[derive(Debug)]
struct TargetEntry {
    relative: String,
    absolute: PathBuf,
    exists: bool,
    is_dir: bool,
}

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("failed to resolve workspace root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid glob pattern '{pattern}': {source}")]
    GlobPattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("failed to compile glob set: {0}")]
    GlobSet(globset::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("embedding error: {0}")]
    Embedding(String),
    #[error("blocking task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub async fn ingest_codebase(params: IngestParams) -> Result<IngestResponse, IngestError> {
    tokio::task::spawn_blocking(move || perform_ingest(params)).await?
}

fn perform_ingest(params: IngestParams) -> Result<IngestResponse, IngestError> {
    let start = Instant::now();

    let IngestParams {
        root,
        include,
        exclude,
        database_name,
        max_file_size_bytes,
        store_file_content,
        paths,
        auto_evict,
        max_database_size_bytes,
        embedding,
    } = params;

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;

    let include_globs = include.unwrap_or_else(|| {
        DEFAULT_INCLUDE_GLOBS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    let exclude_globs = exclude.unwrap_or_else(|| {
        DEFAULT_EXCLUDE_GLOBS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    let database_name = database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string());

    let database_path = absolute_root.join(&database_name);
    let database_path_string = database_path.to_string_lossy().to_string();

    let max_file_size_bytes = max_file_size_bytes.map(|value| value.max(0.0).round() as u64);
    let store_file_content = store_file_content.unwrap_or(true);
    let embedding_config = resolve_embedding_config(embedding)?;
    let auto_evict = auto_evict.unwrap_or(false);
    let max_database_size_bytes = max_database_size_bytes
        .map(|value| value.max(0.0).round() as u64)
        .unwrap_or(DEFAULT_MAX_DATABASE_SIZE_BYTES);

    let root_metadata =
        fs::metadata(&absolute_root).map_err(|source| IngestError::InvalidRoot {
            path: absolute_root.to_string_lossy().to_string(),
            source,
        })?;
    if !root_metadata.is_dir() {
        return Err(IngestError::InvalidRoot {
            path: absolute_root.to_string_lossy().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::Other, "path is not a directory"),
        });
    }

    let target_entries = resolve_target_entries(&absolute_root, paths);
    let using_target_paths = !target_entries.is_empty();
    let target_path_set: HashSet<String> = target_entries
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();

    let scan_outcome = scan_workspace(
        &absolute_root,
        &include_globs,
        &exclude_globs,
        store_file_content,
        max_file_size_bytes,
        if using_target_paths {
            Some(&target_entries)
        } else {
            None
        },
    )?;

    let ScanOutcome {
        files: scanned_files,
        mut skipped,
    } = scan_outcome;
    for entry in &target_entries {
        if !entry.exists {
            skipped.push(SkippedFile {
                path: entry.relative.clone(),
                reason: "target_path_missing".to_string(),
                size: None,
                message: None,
            });
        }
    }

    let now_ms = timestamp_ms();

    let mut conn = Connection::open_with_flags(
        &database_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    ensure_schema(&conn)?;

    let transaction = conn.transaction()?;

    let existing_paths = load_existing_paths(&transaction)?;
    let relevant_existing_paths: HashSet<String> = if using_target_paths {
        existing_paths
            .iter()
            .filter(|path| target_path_set.contains(*path))
            .cloned()
            .collect()
    } else {
        existing_paths.clone()
    };
    let mut retained_paths: HashSet<String> = HashSet::new();
    let mut paths_to_clear: HashSet<String> = HashSet::new();

    let mut chunk_records_by_path: HashMap<String, Vec<ChunkRecord>> = HashMap::new();
    let mut graph_records: HashMap<String, GraphExtraction> = HashMap::new();
    let mut chunk_locations: Vec<(String, usize)> = Vec::new();

    let mut ingested_count = 0usize;

    for file in &scanned_files {
        let path = file.path.clone();
        let size_bytes = file.size as i64;
        let modified = file.modified_ms;
        let db_content = file.stored_content.clone();

        upsert_file(
            &transaction,
            &path,
            size_bytes,
            modified,
            file.hash.clone(),
            now_ms,
            db_content,
        )?;

        retained_paths.insert(path.clone());
        paths_to_clear.insert(path.clone());
        ingested_count += 1;

        if let Some(text) = &file.text_content {
            if embedding_config.enabled {
                let fragments = chunk_content(
                    text,
                    embedding_config.chunk_size_tokens,
                    embedding_config.chunk_overlap_tokens,
                );
                if !fragments.is_empty() {
                    let entry = chunk_records_by_path.entry(path.clone()).or_default();
                    for (index, fragment) in fragments.into_iter().enumerate() {
                        entry.push(ChunkRecord {
                            id: format!("{}:{}", path, index),
                            path: path.clone(),
                            chunk_index: index as i32,
                            content: fragment.content,
                            byte_start: Some(fragment.byte_start as i64),
                            byte_end: Some(fragment.byte_end as i64),
                            line_start: Some(fragment.line_start as i64),
                            line_end: Some(fragment.line_end as i64),
                            embedding: None,
                        });
                        chunk_locations.push((path.clone(), entry.len() - 1));
                    }
                }
            }

            if let Some(extraction) = extract_graph(&path, text) {
                graph_records.insert(path.clone(), extraction);
            }
        }
    }

    let deleted = if using_target_paths {
        target_path_set
            .iter()
            .filter(|path| {
                relevant_existing_paths.contains(*path) && !retained_paths.contains(*path)
            })
            .cloned()
            .collect::<Vec<String>>()
    } else {
        compute_deleted(&existing_paths, &retained_paths)
    };
    let deleted_count = deleted.len();
    remove_deleted(&transaction, &deleted)?;

    let ingestion_id = Uuid::new_v4().to_string();
    let finished_ms = timestamp_ms();

    insert_ingestion_record(
        &transaction,
        &ingestion_id,
        &absolute_root,
        now_ms,
        finished_ms,
        ingested_count,
        skipped.len(),
        deleted_count,
    )?;

    if let Some(commit) = get_current_commit_sha(&absolute_root).ok() {
        upsert_meta(&transaction, "commit_sha", &commit, finished_ms)?;
    }
    upsert_meta(
        &transaction,
        "indexed_at",
        &finished_ms.to_string(),
        finished_ms,
    )?;

    if !paths_to_clear.is_empty() {
        let mut delete_chunks_stmt =
            transaction.prepare("DELETE FROM file_chunks WHERE path = ?1")?;
        let mut delete_nodes_stmt =
            transaction.prepare("DELETE FROM code_graph_nodes WHERE path = ?1")?;
        for path in &paths_to_clear {
            delete_chunks_stmt.execute(params![path])?;
            delete_nodes_stmt.execute(params![path])?;
        }
    }

    let mut graph_node_count = 0usize;
    let mut graph_edge_count = 0usize;

    if !graph_records.is_empty() {
        let mut insert_node_stmt = transaction.prepare(
            "INSERT OR REPLACE INTO code_graph_nodes (id, path, kind, name, signature, range_start, range_end, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut insert_edge_stmt = transaction.prepare(
            "INSERT OR REPLACE INTO code_graph_edges (id, source_id, target_id, type, source_path, target_path, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        for (path, extraction) in &graph_records {
            if !paths_to_clear.contains(path) {
                continue;
            }

            for node in &extraction.nodes {
                let metadata = node
                    .metadata
                    .as_ref()
                    .and_then(|value| serde_json::to_string(value).ok());
                insert_node_stmt.execute(params![
                    &node.id,
                    &node.path,
                    &node.kind,
                    &node.name,
                    &node.signature,
                    &node.range_start,
                    &node.range_end,
                    metadata.as_deref(),
                ])?;
                graph_node_count += 1;
            }

            for edge in &extraction.edges {
                let metadata = edge
                    .metadata
                    .as_ref()
                    .and_then(|value| serde_json::to_string(value).ok());
                insert_edge_stmt.execute(params![
                    &edge.id,
                    &edge.source_id,
                    &edge.target_id,
                    &edge.edge_type,
                    &edge.source_path,
                    &edge.target_path,
                    metadata.as_deref(),
                ])?;
                graph_edge_count += 1;
            }
        }
    }

    let mut embedded_chunk_count = 0usize;
    let mut embedding_model_output: Option<String> = None;

    if embedding_config.enabled && !chunk_locations.is_empty() {
        let mut embedder = create_embedder(&embedding_config)?;
        let texts: Vec<String> = chunk_locations
            .iter()
            .map(|(path, index)| {
                chunk_records_by_path
                    .get(path)
                    .and_then(|records| records.get(*index))
                    .map(|record| record.content.clone())
                    .unwrap_or_default()
            })
            .collect();

        let embeddings = embedder
            .embed(texts, embedding_config.batch_size)
            .map_err(|error| IngestError::Embedding(error.to_string()))?;

        for (idx, embedding_vec) in embeddings.into_iter().enumerate() {
            let (path, record_index) = &chunk_locations[idx];
            if let Some(records) = chunk_records_by_path.get_mut(path) {
                if let Some(record) = records.get_mut(*record_index) {
                    record.embedding = Some(embedding_vec);
                }
            }
        }

        let mut insert_stmt = transaction.prepare(
            "INSERT INTO file_chunks (id, path, chunk_index, content, embedding, embedding_model, byte_start, byte_end, line_start, line_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        )?;

        for records in chunk_records_by_path.values() {
            for record in records {
                if let Some(embedding_vec) = &record.embedding {
                    let blob = embedding_to_bytes(embedding_vec);
                    insert_stmt.execute(params![
                        &record.id,
                        &record.path,
                        record.chunk_index,
                        &record.content,
                        blob,
                        &embedding_config.model,
                        record.byte_start,
                        record.byte_end,
                        record.line_start,
                        record.line_end
                    ])?;
                    embedded_chunk_count += 1;
                }
            }
        }

        if embedded_chunk_count > 0 {
            embedding_model_output = Some(embedding_config.model.clone());
        }
    }

    transaction.commit()?;

    let mut database_size_bytes = fs::metadata(&database_path)
        .map(|meta| meta.len())
        .unwrap_or_default();

    let eviction_report = if auto_evict {
        maybe_auto_evict(&database_path, database_size_bytes, max_database_size_bytes)?
    } else {
        None
    };

    if let Some(report) = &eviction_report {
        database_size_bytes = report.size_after;
    }

    let mut deleted_sorted = deleted;
    deleted_sorted.sort();

    let duration_ms = start.elapsed().as_millis();

    Ok(IngestResponse {
        root: absolute_root.to_string_lossy().to_string(),
        database_path: database_path_string,
        database_size_bytes,
        ingested_file_count: ingested_count,
        skipped,
        deleted_paths: deleted_sorted,
        duration_ms,
        embedded_chunk_count,
        embedding_model: embedding_model_output,
        graph_node_count,
        graph_edge_count,
        evicted: eviction_report,
    })
}

fn resolve_embedding_config(
    params: Option<EmbeddingParams>,
) -> Result<EmbeddingConfig, IngestError> {
    let params = params.unwrap_or_default();
    let enabled = params.enabled.unwrap_or(true);

    let model = params
        .model
        .unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string());

    let chunk_size_tokens = params
        .chunk_size_tokens
        .map(|value| value.max(1) as usize)
        .unwrap_or(DEFAULT_CHUNK_SIZE_TOKENS);

    let chunk_overlap_tokens = params
        .chunk_overlap_tokens
        .map(|value| value.max(0) as usize)
        .unwrap_or(DEFAULT_CHUNK_OVERLAP_TOKENS)
        .min(chunk_size_tokens);

    let batch_size = params
        .batch_size
        .map(|value| value.max(1) as usize)
        .or(Some(DEFAULT_EMBEDDING_BATCH_SIZE));

    Ok(EmbeddingConfig {
        enabled,
        model,
        chunk_size_tokens,
        chunk_overlap_tokens,
        batch_size,
    })
}

fn resolve_target_entries(root: &Path, paths: Option<Vec<String>>) -> Vec<TargetEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    let Some(paths) = paths else {
        return entries;
    };

    for raw in paths {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        let candidate = PathBuf::from(trimmed);
        let absolute = if candidate.is_absolute() {
            candidate.clone()
        } else {
            root.join(&candidate)
        };

        if !absolute.starts_with(root) {
            continue;
        }

        let relative_pathbuf = match absolute.strip_prefix(root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => continue,
        };

        if relative_pathbuf.as_os_str().is_empty() {
            continue;
        }

        let relative = normalize_path(relative_pathbuf.to_string_lossy().as_ref());
        if relative.is_empty() || !seen.insert(relative.clone()) {
            continue;
        }

        let metadata = fs::metadata(&absolute).ok();
        let (exists, is_dir) = match metadata {
            Some(meta) => (true, meta.is_dir()),
            None => (false, false),
        };

        entries.push(TargetEntry {
            relative,
            absolute,
            exists,
            is_dir,
        });
    }

    entries
}

fn maybe_auto_evict(
    database_path: &Path,
    database_size_bytes: u64,
    max_database_size_bytes: u64,
) -> Result<Option<EvictionReport>, IngestError> {
    if max_database_size_bytes == 0 {
        return Ok(None);
    }
    if database_size_bytes <= max_database_size_bytes {
        return Ok(None);
    }

    let target_size = ((max_database_size_bytes as f64) * 0.8).round() as u64;
    let bytes_to_free = database_size_bytes.saturating_sub(target_size);
    if bytes_to_free == 0 {
        return Ok(None);
    }

    let conn = Connection::open(database_path)?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;

    let total_chunks = query_table_count(&conn, "file_chunks")?;
    let total_nodes = query_table_count(&conn, "code_graph_nodes")?;

    let mut evicted_chunks = 0usize;
    let mut evicted_nodes = 0usize;

    if total_chunks > 0 && database_size_bytes > 0 {
        let chunk_count_to_evict = ((bytes_to_free as f64 / database_size_bytes as f64)
            * 0.5
            * total_chunks as f64)
            .ceil() as i64;

        if chunk_count_to_evict > 0 {
            let result = conn.execute(
                "DELETE FROM file_chunks
                 WHERE id IN (
                     SELECT id FROM file_chunks
                     ORDER BY COALESCE(hits, 0) ASC, chunk_index ASC
                     LIMIT ?1
                 )",
                params![chunk_count_to_evict],
            )?;
            evicted_chunks = result as usize;
        }
    }

    let size_after_chunks = match fs::metadata(database_path) {
        Ok(meta) => meta.len(),
        Err(_) => database_size_bytes,
    };

    if total_nodes > 0 && size_after_chunks > target_size && database_size_bytes > 0 {
        let node_count_to_evict = ((size_after_chunks.saturating_sub(target_size) as f64
            / database_size_bytes as f64)
            * 0.3
            * total_nodes as f64)
            .ceil() as i64;

        if node_count_to_evict > 0 {
            let result = conn.execute(
                "DELETE FROM code_graph_nodes
                 WHERE id IN (
                     SELECT id FROM code_graph_nodes
                     ORDER BY COALESCE(hits, 0) ASC
                     LIMIT ?1
                 )",
                params![node_count_to_evict],
            )?;
            evicted_nodes = result as usize;
        }
    }

    conn.execute_batch("VACUUM")?;

    let size_after = match fs::metadata(database_path) {
        Ok(meta) => meta.len(),
        Err(_) => match size_after_chunks {
            0 => 0,
            value => value,
        },
    };

    Ok(Some(EvictionReport {
        database_path: database_path.to_string_lossy().to_string(),
        size_before: database_size_bytes,
        size_after,
        evicted_chunks,
        evicted_nodes,
    }))
}

fn scan_workspace(
    root: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
    store_file_content: bool,
    max_file_size_bytes: Option<u64>,
    target_entries: Option<&[TargetEntry]>,
) -> Result<ScanOutcome, IngestError> {
    let include_globs = compile_globs(include_patterns)?;
    let exclude_globs = compile_globs(exclude_patterns)?;

    let mut files = Vec::new();
    let mut skipped = Vec::new();

    if let Some(entries) = target_entries {
        let mut seen_abs = HashSet::new();
        for entry in entries {
            if !entry.exists {
                continue;
            }

            if !entry.absolute.starts_with(root) {
                continue;
            }

            if !seen_abs.insert(entry.absolute.clone()) {
                continue;
            }

            let walker = build_ignore_walk(&entry.absolute, entry.is_dir);
            collect_files_from_walk(
                root,
                walker,
                include_globs.as_ref(),
                exclude_globs.as_ref(),
                store_file_content,
                max_file_size_bytes,
                &mut files,
                &mut skipped,
            );
        }
    } else {
        let walker = build_ignore_walk(root, true);
        collect_files_from_walk(
            root,
            walker,
            include_globs.as_ref(),
            exclude_globs.as_ref(),
            store_file_content,
            max_file_size_bytes,
            &mut files,
            &mut skipped,
        );
    }

    Ok(ScanOutcome { files, skipped })
}

fn build_ignore_walk(path: &Path, is_dir: bool) -> ignore::Walk {
    let mut builder = WalkBuilder::new(path);
    builder.follow_links(false);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_global(true);
    builder.git_exclude(true);
    if !is_dir {
        builder.max_depth(Some(1));
    }
    builder.build()
}

fn collect_files_from_walk(
    root: &Path,
    walker: ignore::Walk,
    include_globs: Option<&GlobSet>,
    exclude_globs: Option<&GlobSet>,
    store_file_content: bool,
    max_file_size_bytes: Option<u64>,
    files: &mut Vec<ScannedFile>,
    skipped: &mut Vec<SkippedFile>,
) {
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                skipped.push(SkippedFile {
                    path: root.to_string_lossy().to_string(),
                    reason: "walk_error".to_string(),
                    size: None,
                    message: Some(error.to_string()),
                });
                continue;
            }
        };

        if entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false)
        {
            continue;
        }

        let absolute_path = entry.path().to_path_buf();
        let relative_path_buf = absolute_path
            .strip_prefix(root)
            .map(|relative| relative.to_path_buf())
            .unwrap_or_else(|_| absolute_path.clone());
        let relative_path = normalize_path(relative_path_buf.to_string_lossy().as_ref());

        let include_ok = include_globs
            .map(|set| set.is_match(&relative_path_buf))
            .unwrap_or(true);
        if !include_ok {
            continue;
        }

        let is_excluded = exclude_globs
            .map(|set| set.is_match(&relative_path_buf))
            .unwrap_or(false);
        if is_excluded {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                skipped.push(SkippedFile {
                    path: relative_path,
                    reason: "metadata_error".to_string(),
                    size: None,
                    message: Some(error.to_string()),
                });
                continue;
            }
        };

        let size_bytes = metadata.len();
        if let Some(limit) = max_file_size_bytes {
            if size_bytes > limit {
                skipped.push(SkippedFile {
                    path: relative_path,
                    reason: "max_file_size".to_string(),
                    size: Some(size_bytes as f64),
                    message: None,
                });
                continue;
            }
        }

        let bytes = match fs::read(&absolute_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                skipped.push(SkippedFile {
                    path: relative_path,
                    reason: "read_error".to_string(),
                    size: Some(size_bytes as f64),
                    message: Some(error.to_string()),
                });
                continue;
            }
        };

        let hash = hex::encode(Sha256::digest(&bytes));

        let text_content = if is_binary(&bytes) {
            None
        } else {
            Some(String::from_utf8_lossy(&bytes).into_owned())
        };

        let stored_content = if store_file_content {
            text_content.clone()
        } else {
            None
        };

        files.push(ScannedFile {
            path: relative_path,
            size: size_bytes,
            modified_ms: file_modified_to_ms(&metadata),
            hash,
            stored_content,
            text_content,
        });
    }
}

fn compile_globs(patterns: &[String]) -> Result<Option<GlobSet>, IngestError> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|source| IngestError::GlobPattern {
            pattern: pattern.clone(),
            source,
        })?;
        builder.add(glob);
    }

    builder.build().map(Some).map_err(IngestError::GlobSet)
}

fn resolve_root(root: &str) -> Result<PathBuf, IngestError> {
    let candidate = PathBuf::from(root);
    if candidate.is_absolute() {
        return Ok(candidate);
    }

    let cwd = std::env::current_dir().map_err(|source| IngestError::InvalidRoot {
        path: root.to_string(),
        source,
    })?;
    Ok(cwd.join(candidate))
}

fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_millis() as i64
}

fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY,
            size INTEGER NOT NULL,
            modified INTEGER NOT NULL,
            hash TEXT NOT NULL,
            last_indexed_at INTEGER NOT NULL,
            content TEXT
        );
        CREATE TABLE IF NOT EXISTS file_chunks (
            id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            chunk_index INTEGER NOT NULL,
            content TEXT NOT NULL,
            embedding BLOB NOT NULL,
            embedding_model TEXT NOT NULL,
            byte_start INTEGER,
            byte_end INTEGER,
            line_start INTEGER,
            line_end INTEGER,
            hits INTEGER DEFAULT 0,
            FOREIGN KEY (path) REFERENCES files(path) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS ingestions (
            id TEXT PRIMARY KEY,
            root TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            finished_at INTEGER NOT NULL,
            file_count INTEGER NOT NULL,
            skipped_count INTEGER NOT NULL,
            deleted_count INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS files_hash_idx ON files(hash);
        CREATE INDEX IF NOT EXISTS file_chunks_path_idx ON file_chunks(path);
        CREATE TABLE IF NOT EXISTS code_graph_nodes (
            id TEXT PRIMARY KEY,
            path TEXT,
            kind TEXT NOT NULL,
            name TEXT NOT NULL,
            signature TEXT,
            range_start INTEGER,
            range_end INTEGER,
            metadata TEXT,
            hits INTEGER DEFAULT 0,
            UNIQUE(path, kind, name)
        );
        CREATE TABLE IF NOT EXISTS code_graph_edges (
            id TEXT PRIMARY KEY,
            source_id TEXT NOT NULL,
            target_id TEXT NOT NULL,
            type TEXT NOT NULL,
            source_path TEXT,
            target_path TEXT,
            metadata TEXT,
            FOREIGN KEY (source_id) REFERENCES code_graph_nodes(id) ON DELETE CASCADE,
            FOREIGN KEY (target_id) REFERENCES code_graph_nodes(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS code_graph_nodes_path_idx ON code_graph_nodes(path);
        CREATE INDEX IF NOT EXISTS code_graph_edges_source_idx ON code_graph_edges(source_id);
        CREATE INDEX IF NOT EXISTS code_graph_edges_target_idx ON code_graph_edges(target_id);
        "#,
    )
}

fn load_existing_paths(conn: &Transaction<'_>) -> Result<HashSet<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT path FROM files")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for row in rows {
        if let Ok(path) = row {
            set.insert(path);
        }
    }
    Ok(set)
}

fn query_table_count(conn: &Connection, table: &str) -> Result<usize, rusqlite::Error> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as usize)
}

fn upsert_file(
    conn: &Transaction<'_>,
    path: &str,
    size: i64,
    modified: i64,
    hash: String,
    indexed_at: i64,
    content: Option<String>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO files (path, size, modified, hash, last_indexed_at, content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET
            size = excluded.size,
            modified = excluded.modified,
            hash = excluded.hash,
            last_indexed_at = excluded.last_indexed_at,
            content = excluded.content",
        params![path, size, modified, hash, indexed_at, content],
    )?;
    Ok(())
}

fn compute_deleted(existing: &HashSet<String>, retained: &HashSet<String>) -> Vec<String> {
    existing
        .iter()
        .filter(|path| !retained.contains(*path))
        .cloned()
        .collect()
}

fn remove_deleted(conn: &Transaction<'_>, deleted: &[String]) -> Result<(), rusqlite::Error> {
    for path in deleted {
        conn.execute("DELETE FROM files WHERE path = ?1", params![path])?;
    }
    Ok(())
}

fn insert_ingestion_record(
    conn: &Transaction<'_>,
    ingestion_id: &str,
    root: &Path,
    started_at: i64,
    finished_at: i64,
    file_count: usize,
    skipped_count: usize,
    deleted_count: usize,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO ingestions (id, root, started_at, finished_at, file_count, skipped_count, deleted_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            ingestion_id,
            root.to_string_lossy(),
            started_at,
            finished_at,
            file_count as i64,
            skipped_count as i64,
            deleted_count as i64
        ],
    )?;
    Ok(())
}

fn upsert_meta(
    conn: &Transaction<'_>,
    key: &str,
    value: &str,
    updated_at: i64,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO meta (key, value, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at",
        params![key, value, updated_at],
    )?;
    Ok(())
}

fn create_embedder(config: &EmbeddingConfig) -> Result<TextEmbedding, IngestError> {
    let model_name = config.model.trim();
    let parsed = EmbeddingModel::from_str(model_name).map_err(|error| {
        IngestError::Embedding(format!("Unknown embedding model '{model_name}': {error}"))
    })?;
    let options = TextInitOptions::new(parsed).with_show_download_progress(false);

    TextEmbedding::try_new(options).map_err(|error| IngestError::Embedding(error.to_string()))
}

fn embedding_to_bytes(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn get_current_commit_sha(root: &Path) -> Result<String, std::io::Error> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "git rev-parse returned non-zero status",
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "git rev-parse returned empty output",
        ))
    } else {
        Ok(stdout)
    }
}

fn normalize_path(path: &str) -> String {
    path.replace("\\", "/")
}

fn file_modified_to_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().any(|&byte| byte == 0)
}

fn chunk_content(
    content: &str,
    chunk_size_tokens: usize,
    chunk_overlap_tokens: usize,
) -> Vec<ChunkFragment> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let chunk_char_limit = chunk_size_tokens.saturating_mul(4).max(256);
    let overlap_char_limit = chunk_overlap_tokens.saturating_mul(4);

    let mut char_byte_indices: Vec<usize> = Vec::new();
    let mut newline_char_indices: Vec<usize> = Vec::new();
    let mut line_start_char_indices: Vec<usize> = vec![0];

    let mut current_char_index = 0usize;
    for (byte_index, ch) in trimmed.char_indices() {
        char_byte_indices.push(byte_index);
        if ch == '\n' {
            newline_char_indices.push(current_char_index);
            line_start_char_indices.push(current_char_index + 1);
        }
        current_char_index += 1;
    }
    let total_chars = current_char_index;
    let total_bytes = trimmed.len();

    let mut fragments: Vec<ChunkFragment> = Vec::new();
    let mut start = 0usize;

    while start < total_chars {
        let mut end = (start + chunk_char_limit).min(total_chars);

        if end < total_chars {
            let min_break = start + 200;
            if let Some(break_index) = find_break_index(&newline_char_indices, end, min_break) {
                end = break_index + 1;
            }
        }

        let start_byte = char_index_to_byte(start, &char_byte_indices, total_bytes);
        let mut end_byte = char_index_to_byte(end, &char_byte_indices, total_bytes);

        if end_byte < start_byte {
            end_byte = start_byte;
        }

        let raw_slice = &trimmed[start_byte..end_byte];
        let snippet = raw_slice.trim_end();

        if snippet.is_empty() {
            if end <= start {
                break;
            }
            start = end;
            continue;
        }

        let snippet_char_len = snippet.chars().count();
        let effective_end = start + snippet_char_len;
        let effective_end_byte = char_index_to_byte(effective_end, &char_byte_indices, total_bytes);

        let line_start = line_number_for_char(&line_start_char_indices, start);
        let line_end =
            line_number_for_char(&line_start_char_indices, effective_end.saturating_sub(1));

        fragments.push(ChunkFragment {
            content: snippet.to_string(),
            byte_start: start_byte as u32,
            byte_end: effective_end_byte as u32,
            line_start: line_start as u32,
            line_end: line_end as u32,
        });

        if effective_end >= total_chars {
            break;
        }

        let overlap_start = if overlap_char_limit > 0 {
            effective_end.saturating_sub(overlap_char_limit)
        } else {
            effective_end
        };

        start = if overlap_start > start {
            overlap_start
        } else {
            effective_end
        };
    }

    if fragments.is_empty() {
        return vec![fallback_fragment(trimmed)];
    }

    fragments
}

fn fallback_fragment(content: &str) -> ChunkFragment {
    let snippet = content.trim();
    if snippet.is_empty() {
        return ChunkFragment {
            content: String::new(),
            byte_start: 0,
            byte_end: 0,
            line_start: 1,
            line_end: 1,
        };
    }

    let byte_length = snippet.as_bytes().len() as u32;
    let line_count = snippet.lines().count().max(1) as u32;

    ChunkFragment {
        content: snippet.to_string(),
        byte_start: 0,
        byte_end: byte_length,
        line_start: 1,
        line_end: line_count,
    }
}

fn char_index_to_byte(index: usize, char_byte_indices: &[usize], total_bytes: usize) -> usize {
    if index == char_byte_indices.len() {
        total_bytes
    } else {
        char_byte_indices.get(index).copied().unwrap_or(total_bytes)
    }
}

fn find_break_index(newlines: &[usize], end: usize, min_break: usize) -> Option<usize> {
    if newlines.is_empty() {
        return None;
    }

    let mut lo = 0i64;
    let mut hi = (newlines.len() as i64) - 1;
    let mut candidate: Option<usize> = None;

    while lo <= hi {
        let mid = ((lo + hi) / 2) as usize;
        let value = newlines[mid];
        if value < end {
            candidate = Some(value);
            lo = mid as i64 + 1;
        } else {
            hi = mid as i64 - 1;
        }
    }

    if let Some(value) = candidate {
        if value >= min_break {
            return Some(value);
        }
    }

    None
}

fn line_number_for_char(line_starts: &[usize], target: usize) -> usize {
    if line_starts.is_empty() {
        return 1;
    }

    let mut lo = 0i64;
    let mut hi = (line_starts.len() as i64) - 1;
    let mut index = 0usize;

    while lo <= hi {
        let mid = ((lo + hi) / 2) as usize;
        let value = line_starts[mid];
        if value <= target {
            index = mid;
            lo = mid as i64 + 1;
        } else {
            hi = mid as i64 - 1;
        }
    }

    index + 1
}
