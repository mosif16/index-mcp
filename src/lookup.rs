use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub content: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub path: String,
    pub snippet: Option<String>,
}

pub fn fetch_file(database_path: &Path, path: &str) -> Result<Option<FileEntry>> {
    if !database_path.exists() {
        return Ok(None);
    }

    let connection = Connection::open(database_path)
        .with_context(|| format!("Failed to open database at {:?}", database_path))?;
    let mut stmt = connection
        .prepare("SELECT path, size, modified, content FROM files WHERE path = ?1 LIMIT 1")
        .context("Failed to prepare file lookup statement")?;

    let result = stmt
        .query_row(params![path], |row| {
            Ok(FileEntry {
                path: row.get(0)?,
                size: row.get::<_, i64>(1)? as u64,
                modified: row
                    .get::<_, Option<i64>>(2)?
                    .map(|value| Utc.timestamp_millis_opt(value).single())
                    .flatten(),
                content: row.get::<_, Option<String>>(3)?,
            })
        })
        .optional()
        .context("Failed to read file entry")?;

    Ok(result)
}

pub fn search_files(database_path: &Path, query: &str, limit: usize) -> Result<Vec<SearchMatch>> {
    if !database_path.exists() {
        return Ok(Vec::new());
    }

    let connection = Connection::open(database_path)
        .with_context(|| format!("Failed to open database at {:?}", database_path))?;
    let mut stmt = connection
        .prepare(
            "SELECT path, content FROM files WHERE content IS NOT NULL AND content LIKE ?1 ESCAPE '\' LIMIT ?2",
        )
        .context("Failed to prepare search query")?;

    let pattern = format!("%{}%", escape_like(query));
    let rows = stmt
        .query_map(params![pattern, limit as i64], |row| {
            let path: String = row.get(0)?;
            let content: Option<String> = row.get(1)?;
            let snippet = content
                .as_deref()
                .and_then(|text| create_snippet(text, query));
            Ok(SearchMatch { path, snippet })
        })
        .context("Failed to execute search query")?;

    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }

    Ok(matches)
}

fn escape_like(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch == '%' || ch == '_' || ch == '\\' {
            escaped.push('\\');
            escaped.push(ch);
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

fn create_snippet(content: &str, query: &str) -> Option<String> {
    let needle = query.to_lowercase();
    let haystack = content.to_lowercase();
    let position = haystack.find(&needle)?;
    let start = position.saturating_sub(120);
    let end = usize::min(position + needle.len() + 120, content.len());
    let snippet = content[start..end].replace('\n', " ");
    Some(snippet)
}
