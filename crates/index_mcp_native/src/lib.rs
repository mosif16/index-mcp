use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{Error as IgnoreError, WalkBuilder};
use napi::bindgen_prelude::*;
use napi::Result;
use napi_derive::napi;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

mod chunk;
mod embedding;

pub use embedding::{clear_embedding_cache, generate_embeddings};

#[napi(object)]
pub struct ScanOptions {
    pub root: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_file_size_bytes: Option<f64>,
    pub needs_content: bool,
}

#[napi(object)]
pub struct MetadataOptions {
    pub root: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_file_size_bytes: Option<f64>,
}

#[napi(object)]
pub struct ReadRepoOptions {
    pub root: String,
    pub paths: Vec<String>,
    pub max_file_size_bytes: Option<f64>,
    pub needs_content: bool,
}

#[napi(object)]
pub struct NativeFileEntry {
    pub path: String,
    pub size: f64,
    pub modified: f64,
    pub hash: String,
    pub content: Option<String>,
    pub is_binary: bool,
}

#[napi(object)]
pub struct NativeMetadataEntry {
    pub path: String,
    pub size: f64,
    pub modified: f64,
}

#[napi(object)]
pub struct NativeSkippedFile {
    pub path: String,
    pub reason: String,
    pub size: Option<f64>,
    pub message: Option<String>,
}

#[napi(object)]
pub struct NativeScanResult {
    pub files: Vec<NativeFileEntry>,
    pub skipped: Vec<NativeSkippedFile>,
}

#[napi(object)]
pub struct NativeMetadataResult {
    pub entries: Vec<NativeMetadataEntry>,
    pub skipped: Vec<NativeSkippedFile>,
}

#[napi(object)]
pub struct NativeReadResult {
    pub files: Vec<NativeFileEntry>,
    pub skipped: Vec<NativeSkippedFile>,
}

#[napi(object)]
pub struct NativeChunkFragment {
    pub content: String,
    pub byte_start: f64,
    pub byte_end: f64,
    pub line_start: f64,
    pub line_end: f64,
}

#[napi(object)]
pub struct AnalyzeOptions {
    pub path: String,
    pub content: String,
    pub chunk_size_tokens: Option<f64>,
    pub chunk_overlap_tokens: Option<f64>,
}

#[napi(object)]
pub struct NativeAnalysisResult {
    pub chunks: Vec<NativeChunkFragment>,
}

struct ScanJob {
    relative_path: String,
}

enum ScanOutcome {
    File(NativeFileEntry),
    Skipped(NativeSkippedFile),
}

enum MetadataOutcome {
    Entry(NativeMetadataEntry),
    Skipped(NativeSkippedFile),
}

#[napi]
pub fn scan_repo(options: ScanOptions) -> Result<NativeScanResult> {
    let root_path = PathBuf::from(&options.root);
    if !root_path.is_dir() {
        return Err(Error::from_reason(format!(
            "scan_repo root must be a directory: {}",
            options.root
        )));
    }

    let include_globset = build_globset(&options.include)?;
    let exclude_globset = build_globset(&options.exclude)?;

    let (jobs, mut skipped) = collect_scan_jobs(&root_path, &include_globset, &exclude_globset);

    let max_file_size = options.max_file_size_bytes.map(|value| value as u64);
    let needs_content = options.needs_content;

    let outcomes: Vec<ScanOutcome> = jobs
        .into_par_iter()
        .map(|job| process_file(&root_path, job, max_file_size, needs_content))
        .collect();

    let mut files: Vec<NativeFileEntry> = Vec::new();

    for outcome in outcomes {
        match outcome {
            ScanOutcome::File(file) => files.push(file),
            ScanOutcome::Skipped(file) => skipped.push(file),
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(NativeScanResult { files, skipped })
}

#[napi]
pub fn scan_repo_metadata(options: MetadataOptions) -> Result<NativeMetadataResult> {
    let root_path = PathBuf::from(&options.root);
    if !root_path.is_dir() {
        return Err(Error::from_reason(format!(
            "scan_repo_metadata root must be a directory: {}",
            options.root
        )));
    }

    let include_globset = build_globset(&options.include)?;
    let exclude_globset = build_globset(&options.exclude)?;

    let (jobs, mut skipped) = collect_scan_jobs(&root_path, &include_globset, &exclude_globset);

    let max_file_size = options.max_file_size_bytes.map(|value| value as u64);

    let outcomes: Vec<MetadataOutcome> = jobs
        .par_iter()
        .map(|job| process_file_metadata(&root_path, job, max_file_size))
        .collect();

    let mut entries: Vec<NativeMetadataEntry> = Vec::new();

    for outcome in outcomes {
        match outcome {
            MetadataOutcome::Entry(entry) => entries.push(entry),
            MetadataOutcome::Skipped(file) => skipped.push(file),
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(NativeMetadataResult { entries, skipped })
}

#[napi]
pub fn read_repo_files(options: ReadRepoOptions) -> Result<NativeReadResult> {
    let root_path = PathBuf::from(&options.root);
    if !root_path.is_dir() {
        return Err(Error::from_reason(format!(
            "read_repo_files root must be a directory: {}",
            options.root
        )));
    }

    let mut unique_paths: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    for path in options.paths.iter() {
        if let Some(normalized) = sanitize_requested_path(path) {
            if seen.insert(normalized.clone()) {
                unique_paths.push(normalized);
            }
        }
    }

    let max_file_size = options.max_file_size_bytes.map(|value| value as u64);
    let needs_content = options.needs_content;

    let outcomes: Vec<ScanOutcome> = unique_paths
        .into_par_iter()
        .map(|relative_path| {
            let job = ScanJob { relative_path };
            process_file(&root_path, job, max_file_size, needs_content)
        })
        .collect();

    let mut files: Vec<NativeFileEntry> = Vec::new();
    let mut skipped: Vec<NativeSkippedFile> = Vec::new();

    for outcome in outcomes {
        match outcome {
            ScanOutcome::File(file) => files.push(file),
            ScanOutcome::Skipped(file) => skipped.push(file),
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(NativeReadResult { files, skipped })
}

#[napi]
pub fn analyze_file_content(options: AnalyzeOptions) -> Result<NativeAnalysisResult> {
    let chunk_size = options.chunk_size_tokens.unwrap_or(256.0).max(0.0).floor() as usize;
    let chunk_overlap = options
        .chunk_overlap_tokens
        .unwrap_or(32.0)
        .max(0.0)
        .floor() as usize;

    let fragments = chunk::chunk_content(&options.content, chunk_size, chunk_overlap);

    let chunks = fragments
        .into_iter()
        .map(|fragment| NativeChunkFragment {
            content: fragment.content,
            byte_start: fragment.byte_start as f64,
            byte_end: fragment.byte_end as f64,
            line_start: fragment.line_start as f64,
            line_end: fragment.line_end as f64,
        })
        .collect();

    Ok(NativeAnalysisResult { chunks })
}

fn collect_scan_jobs(
    root_path: &Path,
    include_globset: &Option<GlobSet>,
    exclude_globset: &Option<GlobSet>,
) -> (Vec<ScanJob>, Vec<NativeSkippedFile>) {
    let mut jobs: Vec<ScanJob> = Vec::new();
    let mut skipped: Vec<NativeSkippedFile> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for result in WalkBuilder::new(root_path)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .standard_filters(true)
        .build()
    {
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                let path = extract_error_path(&err)
                    .and_then(|p| to_relative_posix(root_path, p))
                    .unwrap_or_else(|| String::from("."));
                skipped.push(NativeSkippedFile {
                    path,
                    reason: "read-error".to_string(),
                    size: None,
                    message: Some(err.to_string()),
                });
                continue;
            }
        };

        if entry.depth() == 0 {
            continue;
        }

        if let Some(file_type) = entry.file_type() {
            if !file_type.is_file() {
                continue;
            }
        } else {
            continue;
        }

        if let Some(relative) = to_relative_posix(root_path, entry.path()) {
            if seen_paths.contains(&relative) {
                continue;
            }

            if let Some(exclude) = exclude_globset {
                if exclude.is_match(&relative) {
                    continue;
                }
            }

            if let Some(include) = include_globset {
                if !include.is_match(&relative) {
                    continue;
                }
            }

            seen_paths.insert(relative.clone());
            jobs.push(ScanJob {
                relative_path: relative,
            });
        }
    }

    (jobs, skipped)
}

fn process_file(
    root: &Path,
    job: ScanJob,
    max_file_size: Option<u64>,
    needs_content: bool,
) -> ScanOutcome {
    let absolute_path = root.join(&job.relative_path);
    let metadata = match fs::metadata(&absolute_path) {
        Ok(meta) => meta,
        Err(err) => {
            return ScanOutcome::Skipped(NativeSkippedFile {
                path: job.relative_path,
                reason: "read-error".to_string(),
                size: None,
                message: Some(err.to_string()),
            });
        }
    };

    let file_size = metadata.len();
    if let Some(limit) = max_file_size {
        if file_size > limit {
            return ScanOutcome::Skipped(NativeSkippedFile {
                path: job.relative_path,
                reason: "file-too-large".to_string(),
                size: Some(file_size as f64),
                message: None,
            });
        }
    }

    let modified = metadata
        .modified()
        .ok()
        .and_then(system_time_to_millis)
        .unwrap_or(0);

    let bytes = match fs::read(&absolute_path) {
        Ok(content) => content,
        Err(err) => {
            return ScanOutcome::Skipped(NativeSkippedFile {
                path: job.relative_path,
                reason: "read-error".to_string(),
                size: Some(file_size as f64),
                message: Some(err.to_string()),
            });
        }
    };

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = format!("{:x}", hasher.finalize());

    let is_binary = bytes.iter().take(1024).any(|b| *b == 0);

    let content = if needs_content && !is_binary {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        None
    };

    ScanOutcome::File(NativeFileEntry {
        path: job.relative_path,
        size: file_size as f64,
        modified: modified as f64,
        hash,
        content,
        is_binary,
    })
}

fn process_file_metadata(
    root: &Path,
    job: &ScanJob,
    max_file_size: Option<u64>,
) -> MetadataOutcome {
    let absolute_path = root.join(&job.relative_path);
    let metadata = match fs::metadata(&absolute_path) {
        Ok(meta) => meta,
        Err(err) => {
            return MetadataOutcome::Skipped(NativeSkippedFile {
                path: job.relative_path.clone(),
                reason: "read-error".to_string(),
                size: None,
                message: Some(err.to_string()),
            });
        }
    };

    let file_size = metadata.len();
    if let Some(limit) = max_file_size {
        if file_size > limit {
            return MetadataOutcome::Skipped(NativeSkippedFile {
                path: job.relative_path.clone(),
                reason: "file-too-large".to_string(),
                size: Some(file_size as f64),
                message: None,
            });
        }
    }

    let modified = metadata
        .modified()
        .ok()
        .and_then(system_time_to_millis)
        .unwrap_or(0);

    MetadataOutcome::Entry(NativeMetadataEntry {
        path: job.relative_path.clone(),
        size: file_size as f64,
        modified: modified as f64,
    })
}

fn sanitize_requested_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut normalized = trimmed.replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized.trim_start_matches("./").to_string();
    }
    while normalized.starts_with('/') {
        normalized.remove(0);
    }

    if normalized.is_empty() {
        return None;
    }

    if normalized
        .split('/')
        .any(|segment| segment == ".." || segment.is_empty())
    {
        return None;
    }

    let filtered_segments: Vec<&str> = normalized
        .split('/')
        .filter(|segment| *segment != ".")
        .collect();

    if filtered_segments.is_empty() {
        return None;
    }

    Some(filtered_segments.join("/"))
}

fn system_time_to_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn extract_error_path<'a>(error: &'a IgnoreError) -> Option<&'a Path> {
    match error {
        IgnoreError::Partial(errors) => errors.iter().find_map(extract_error_path),
        IgnoreError::WithLineNumber { err, .. } => extract_error_path(err),
        IgnoreError::WithPath { path, .. } => Some(path.as_path()),
        IgnoreError::WithDepth { err, .. } => extract_error_path(err),
        IgnoreError::Loop { child, .. } => Some(child.as_path()),
        IgnoreError::Io(_)
        | IgnoreError::Glob { .. }
        | IgnoreError::UnrecognizedFileType(_)
        | IgnoreError::InvalidDefinition => None,
    }
}

fn to_relative_posix(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut relative_str = relative.to_string_lossy().replace('\\', "/");
    if relative_str.starts_with("./") {
        relative_str = relative_str.trim_start_matches("./").to_string();
    }
    if relative_str.starts_with('/') {
        relative_str = relative_str.trim_start_matches('/').to_string();
    }
    if relative_str.is_empty() {
        None
    } else {
        Some(relative_str)
    }
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    let mut added = false;
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = trimmed.replace('\\', "/");
        let glob = GlobBuilder::new(&normalized)
            .literal_separator(false)
            .build()
            .map_err(|err| {
                Error::from_reason(format!("Invalid glob pattern '{}': {}", pattern, err))
            })?;
        builder.add(glob);
        added = true;
    }

    if !added {
        return Ok(None);
    }

    builder
        .build()
        .map(Some)
        .map_err(|err| Error::from_reason(format!("Failed to build globset: {}", err)))
}
