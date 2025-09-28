use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub hash: String,
    pub content: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRun {
    pub id: Uuid,
    pub root: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub file_count: usize,
    pub skipped_count: usize,
    pub store_content: bool,
}

pub fn write_snapshot(path: &Path, files: &[FileRecord], run: &IngestRun) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }

    let connection =
        Connection::open(path).with_context(|| format!("Failed to open database at {:?}", path))?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                modified INTEGER,
                hash TEXT NOT NULL,
                content TEXT
            );
            CREATE TABLE IF NOT EXISTS ingestions (
                id TEXT PRIMARY KEY,
                root TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                finished_at INTEGER NOT NULL,
                file_count INTEGER NOT NULL,
                skipped_count INTEGER NOT NULL,
                store_content INTEGER NOT NULL
            );",
        )
        .context("Failed to initialise schema")?;

    let transaction = connection
        .transaction()
        .context("Failed to open transaction")?;

    transaction
        .execute("DELETE FROM files", [])
        .context("Failed to clear previous file records")?;

    let mut insert_stmt = transaction
        .prepare(
            "INSERT INTO files (path, size, modified, hash, content) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .context("Failed to prepare insert statement")?;

    for file in files {
        insert_stmt
            .execute(params![
                &file.path,
                file.size as i64,
                file.modified.map(|dt| dt.timestamp_millis()),
                &file.hash,
                file.content.as_deref()
            ])
            .with_context(|| format!("Failed to insert file record for {}", file.path))?;
    }

    transaction
        .execute(
            "INSERT OR REPLACE INTO ingestions (id, root, started_at, finished_at, file_count, skipped_count, store_content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id.to_string(),
                &run.root,
                run.started_at.timestamp_millis(),
                run.finished_at.timestamp_millis(),
                run.file_count as i64,
                run.skipped_count as i64,
                if run.store_content { 1 } else { 0 },
            ],
        )
        .context("Failed to insert ingestion record")?;

    transaction
        .commit()
        .context("Failed to commit ingestion snapshot")?;

    Ok(())
}
