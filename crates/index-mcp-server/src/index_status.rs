use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use thiserror::Error;

/// Default SQLite filename used by the legacy Node implementation.
pub const DEFAULT_DB_FILENAME: &str = ".mcp-index.sqlite";
const DEFAULT_HISTORY_LIMIT: u32 = 5;

#[derive(Debug, Clone, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatusParams {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub database_name: Option<String>,
    #[serde(default)]
    pub history_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatusIngestion {
    pub id: String,
    pub root: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_ms: i64,
    pub file_count: i64,
    pub skipped_count: i64,
    pub deleted_count: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatusResponse {
    pub database_path: String,
    pub database_exists: bool,
    pub database_size_bytes: Option<u64>,
    pub total_files: u64,
    pub total_chunks: u64,
    pub embedding_models: Vec<String>,
    pub total_graph_nodes: u64,
    pub total_graph_edges: u64,
    pub latest_ingestion: Option<IndexStatusIngestion>,
    pub recent_ingestions: Vec<IndexStatusIngestion>,
    pub commit_sha: Option<String>,
    pub indexed_at: Option<i64>,
    pub current_commit_sha: Option<String>,
    pub is_stale: bool,
}

#[derive(Debug, Error)]
pub enum IndexStatusError {
    #[error("failed to resolve workspace root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to access {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("failed to query git commit: {0}")]
    Git(#[from] std::io::Error),
    #[error("blocking task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub async fn get_index_status(
    params: IndexStatusParams,
) -> Result<IndexStatusResponse, IndexStatusError> {
    tokio::task::spawn_blocking(move || compute_index_status(params)).await?
}

fn compute_index_status(
    params: IndexStatusParams,
) -> Result<IndexStatusResponse, IndexStatusError> {
    let root = params.root.unwrap_or_else(|| "./".to_string());
    let history_limit = params.history_limit.unwrap_or(DEFAULT_HISTORY_LIMIT) as usize;
    let database_name = params
        .database_name
        .unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string());

    let absolute_root = resolve_root(&root)?;
    let database_path = absolute_root.join(&database_name);
    let database_path_string = database_path.to_string_lossy().to_string();

    let metadata = match fs::metadata(&database_path) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(IndexStatusError::Io {
                path: database_path_string,
                source: err,
            });
        }
    };

    if metadata.is_none() {
        let current_commit_sha = get_current_commit_sha(&absolute_root).ok();
        return Ok(IndexStatusResponse {
            database_path: database_path_string,
            database_exists: false,
            database_size_bytes: None,
            total_files: 0,
            total_chunks: 0,
            embedding_models: Vec::new(),
            total_graph_nodes: 0,
            total_graph_edges: 0,
            latest_ingestion: None,
            recent_ingestions: Vec::new(),
            commit_sha: None,
            indexed_at: None,
            current_commit_sha,
            is_stale: true,
        });
    }

    let database_size_bytes = metadata.map(|m| m.len());
    let current_commit_sha = get_current_commit_sha(&absolute_root).ok();

    let conn = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let total_files = query_count(&conn, "SELECT COUNT(*) FROM files")?;
    let total_chunks = query_count(&conn, "SELECT COUNT(*) FROM file_chunks")?;
    let total_graph_nodes = query_count(&conn, "SELECT COUNT(*) FROM code_graph_nodes")?;
    let total_graph_edges = query_count(&conn, "SELECT COUNT(*) FROM code_graph_edges")?;

    let embedding_models = query_embedding_models(&conn)?;
    let commit_sha = query_meta_value(&conn, "commit_sha");
    let indexed_at =
        query_meta_value(&conn, "indexed_at").and_then(|value| value.parse::<i64>().ok());
    let ingestions = query_ingestions(&conn, history_limit)?;
    let latest_ingestion = ingestions.first().cloned();

    let is_stale = matches!((&current_commit_sha, &commit_sha), (Some(current), Some(stored)) if current != stored);

    Ok(IndexStatusResponse {
        database_path: database_path_string,
        database_exists: true,
        database_size_bytes,
        total_files,
        total_chunks,
        embedding_models,
        total_graph_nodes,
        total_graph_edges,
        latest_ingestion,
        recent_ingestions: ingestions,
        commit_sha,
        indexed_at,
        current_commit_sha,
        is_stale,
    })
}

fn resolve_root(root: &str) -> Result<PathBuf, IndexStatusError> {
    let candidate = PathBuf::from(root);
    if candidate.is_absolute() {
        Ok(candidate)
    } else {
        let current_dir =
            std::env::current_dir().map_err(|source| IndexStatusError::InvalidRoot {
                path: root.to_string(),
                source,
            })?;
        Ok(current_dir.join(candidate))
    }
}

fn query_count(conn: &Connection, sql: &str) -> Result<u64, rusqlite::Error> {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count.max(0) as u64)
}

fn query_embedding_models(conn: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT embedding_model FROM file_chunks WHERE embedding_model IS NOT NULL ORDER BY embedding_model ASC",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let models = rows.flatten().filter(|model| !model.is_empty()).collect();
    Ok(models)
}

fn query_meta_value(conn: &Connection, key: &str) -> Option<String> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1").ok()?;
    stmt.query_row(params![key], |row| row.get::<_, String>(0))
        .ok()
}

fn query_ingestions(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<IndexStatusIngestion>, rusqlite::Error> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT
            id,
            root,
            started_at,
            finished_at,
            file_count,
            skipped_count,
            deleted_count
        FROM ingestions
        ORDER BY finished_at DESC
        LIMIT ?1",
    )?;

    let rows = stmt.query_map(params![limit as i64], |row| {
        let started_at: i64 = row.get(2)?;
        let finished_at: i64 = row.get(3)?;
        Ok(IndexStatusIngestion {
            id: row.get(0)?,
            root: row.get(1)?,
            started_at,
            finished_at,
            duration_ms: finished_at - started_at,
            file_count: row.get(4)?,
            skipped_count: row.get(5)?,
            deleted_count: row.get(6)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn get_current_commit_sha(root: &Path) -> Result<String, std::io::Error> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(root)
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::other(
            "git rev-parse returned non-zero status",
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Err(std::io::Error::other("git rev-parse returned empty output"))
    } else {
        Ok(stdout)
    }
}
