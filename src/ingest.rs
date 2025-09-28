use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::database::{self, FileRecord, IngestRun};

const DEFAULT_MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;
const DEFAULT_DATABASE_NAME: &str = ".mcp-index.sqlite";

pub struct IngestOptions {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub database_name: String,
    pub max_file_size: Option<u64>,
    pub store_content: bool,
}

impl Default for IngestOptions {
    fn default() -> Self {
        IngestOptions {
            include: vec!["**/*".to_string()],
            exclude: Vec::new(),
            database_name: DEFAULT_DATABASE_NAME.to_string(),
            max_file_size: Some(DEFAULT_MAX_FILE_SIZE),
            store_content: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IngestSummary {
    pub run: IngestRun,
    pub database_path: PathBuf,
    pub total_bytes: u64,
    pub duration_ms: u128,
}

pub fn ingest_codebase(root: &Path, options: IngestOptions) -> Result<IngestSummary> {
    if !root.exists() {
        return Err(anyhow!("Ingest root {:?} does not exist", root));
    }

    let database_name = if options.database_name.is_empty() {
        DEFAULT_DATABASE_NAME.to_string()
    } else {
        options.database_name
    };

    let root = root
        .canonicalize()
        .with_context(|| format!("Failed to resolve {:?}", root))?;
    let db_path = root.join(&database_name);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }

    let mut include = options.include;
    if include.is_empty() {
        include.push("**/*".to_string());
    }

    let mut exclude = options.exclude;
    exclude.extend(default_excludes());
    exclude.push(database_name.clone());
    exclude.push(format!("{}-journal", database_name));

    let include_set = compile_globs(&include)?;
    let exclude_set = compile_globs(&exclude)?;

    let max_size = options.max_file_size.unwrap_or(DEFAULT_MAX_FILE_SIZE);

    let start_time = Instant::now();
    let started_at = Utc::now();

    let mut records: Vec<FileRecord> = Vec::new();
    let mut skipped = 0usize;
    let mut total_bytes = 0u64;

    let walker = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let relative = match entry.path().strip_prefix(&root) {
                Ok(path) => path,
                Err(_) => return false,
            };
            let rel_str = path_to_string(relative);
            if rel_str.is_empty() {
                return true;
            }
            !exclude_set.is_match(&rel_str)
        });

    for entry in walker {
        let entry = match entry {
            Ok(value) => value,
            Err(error) => {
                skipped += 1;
                eprintln!("[index-mcp] Failed to walk entry: {error}");
                continue;
            }
        };

        if entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(&root)
            .map_err(|_| anyhow!("Failed to compute relative path"))?;
        let rel_str = path_to_string(relative);
        if rel_str.is_empty() {
            continue;
        }

        if exclude_set.is_match(&rel_str) {
            skipped += 1;
            continue;
        }

        if !include_set.is_match(&rel_str) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(meta) => meta,
            Err(error) => {
                skipped += 1;
                eprintln!("[index-mcp] Failed to read metadata for {rel_str}: {error}");
                continue;
            }
        };

        let file_size = metadata.len();
        if file_size > max_size {
            skipped += 1;
            eprintln!("[index-mcp] Skipping {rel_str} (size {file_size} exceeds max {max_size})");
            continue;
        }

        let modified = metadata.modified().ok().and_then(to_datetime);

        let data = match fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(error) => {
                skipped += 1;
                eprintln!("[index-mcp] Failed to read {rel_str}: {error}");
                continue;
            }
        };

        total_bytes += file_size;

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let hash = format!("{:x}", hasher.finalize());

        let content = if options.store_content && !is_binary(&data) {
            match String::from_utf8(data) {
                Ok(text) => Some(text),
                Err(_) => None,
            }
        } else {
            None
        };

        records.push(FileRecord {
            path: rel_str,
            size: file_size,
            modified,
            hash,
            content,
        });
    }

    let finished_at = Utc::now();
    let run = IngestRun {
        id: Uuid::new_v4(),
        root: root.to_string_lossy().into_owned(),
        started_at,
        finished_at,
        file_count: records.len(),
        skipped_count: skipped,
        store_content: options.store_content,
    };

    database::write_snapshot(&db_path, &records, &run)?;

    Ok(IngestSummary {
        run,
        database_path: db_path,
        total_bytes,
        duration_ms: start_time.elapsed().as_millis(),
    })
}

fn compile_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).with_context(|| format!("Invalid glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| anyhow!("Failed to build glob set: {error}"))
}

fn default_excludes() -> Vec<String> {
    vec![
        ".git/**".to_string(),
        "dist/**".to_string(),
        "target/**".to_string(),
        "node_modules/**".to_string(),
        "build/**".to_string(),
        "logs/**".to_string(),
        "tmp/**".to_string(),
    ]
}

fn path_to_string(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn to_datetime(time: std::time::SystemTime) -> Option<DateTime<Utc>> {
    let duration = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(
        Utc.timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
            .single()?,
    )
}

fn is_binary(data: &[u8]) -> bool {
    data.iter().any(|byte| *byte == 0)
}
