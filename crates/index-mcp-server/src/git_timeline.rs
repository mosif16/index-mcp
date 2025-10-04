use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use regex::Regex;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::index_status::DEFAULT_DB_FILENAME;

const GIT_LOG_FIELD_SEPARATOR: &str = "\u{001f}";
const GIT_LOG_RECORD_SEPARATOR: &str = "\u{001e}";
const DIFF_PREVIEW_MAX_LINES: usize = 200;
const DIFF_PREVIEW_MAX_CHARS: usize = 4_000;

static RELATIVE_SINCE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d+)\s*(d|w|m|y)$").expect("valid regex"));
static PR_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"\(#(\d+)\)").expect("valid regex"),
        Regex::new(r"PR\s*#(\d+)").expect("valid regex"),
        Regex::new(r"#(\d+)").expect("valid regex"),
    ]
});

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineParams {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub database_name: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub include_merges: Option<bool>,
    #[serde(default)]
    pub include_file_stats: Option<bool>,
    #[serde(default)]
    pub include_diffs: Option<bool>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub diff_pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineFileChange {
    pub path: String,
    pub insertions: Option<i64>,
    pub deletions: Option<i64>,
    pub net: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineTopFile {
    pub path: String,
    pub insertions: i64,
    pub deletions: i64,
    pub net: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineDirectoryChurn {
    pub path: String,
    pub insertions: i64,
    pub deletions: i64,
    pub net: i64,
    pub files_changed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineDiffSummary {
    pub files_changed: usize,
    pub insertions: i64,
    pub deletions: i64,
    pub net: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineEntry {
    pub sha: String,
    pub subject: String,
    pub summary: String,
    pub author: TimelineIdentity,
    pub author_date: String,
    pub committer: TimelineIdentity,
    pub committer_date: String,
    pub parents: Vec<String>,
    pub is_merge: bool,
    pub pull_request_number: Option<i64>,
    pub files_changed: usize,
    pub insertions: i64,
    pub deletions: i64,
    pub file_changes: Vec<RepositoryTimelineFileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_pointer: Option<String>,
    pub top_files: Vec<RepositoryTimelineTopFile>,
    pub directory_churn: Vec<RepositoryTimelineDirectoryChurn>,
    pub diff_summary: RepositoryTimelineDiffSummary,
    pub highlights: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TimelineIdentity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineResponse {
    pub repository_root: String,
    pub branch: String,
    pub limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    pub include_merges: bool,
    pub include_file_stats: bool,
    pub include_diffs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_pattern: Option<String>,
    pub total_commits: usize,
    pub merge_commits: usize,
    pub total_insertions: i64,
    pub total_deletions: i64,
    pub entries: Vec<RepositoryTimelineEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineEntryLookupParams {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub database_name: Option<String>,
    pub commit_sha: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryTimelineEntryLookupResponse {
    pub database_path: String,
    pub entry: RepositoryTimelineEntry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Error)]
pub enum RepositoryTimelineError {
    #[error("failed to resolve workspace root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("git repository not found at '{path}': {message}")]
    NotAGitRepository { path: String, message: String },
    #[error("git command failed: {0}")]
    Git(String),
    #[error("blocking task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("failed to persist timeline entries to '{path}': {source}")]
    Database {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("failed to serialize repository timeline entry: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("timeline entry '{commit_sha}' not found in database '{path}'")]
    EntryNotFound { commit_sha: String, path: String },
}

pub async fn repository_timeline(
    params: RepositoryTimelineParams,
) -> Result<RepositoryTimelineResponse, RepositoryTimelineError> {
    tokio::task::spawn_blocking(move || perform_repository_timeline(params)).await?
}

pub async fn repository_timeline_entry_detail(
    params: RepositoryTimelineEntryLookupParams,
) -> Result<RepositoryTimelineEntryLookupResponse, RepositoryTimelineError> {
    tokio::task::spawn_blocking(move || fetch_repository_timeline_entry(params)).await?
}

fn perform_repository_timeline(
    params: RepositoryTimelineParams,
) -> Result<RepositoryTimelineResponse, RepositoryTimelineError> {
    let RepositoryTimelineParams {
        root,
        database_name,
        branch,
        limit,
        since,
        include_merges,
        include_file_stats,
        include_diffs,
        paths,
        diff_pattern,
    } = params;

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;
    let repo_root = verify_git_repository(&absolute_root)?;

    let remote_url = normalize_remote_url(resolve_remote_url(&repo_root)?);

    let branch_name = branch.unwrap_or_else(|| "HEAD".to_string());

    let log_output = run_git_log(
        &repo_root,
        GitLogOptions {
            branch: &branch_name,
            limit: limit.unwrap_or(20),
            since: since.as_deref(),
            include_merges: include_merges.unwrap_or(true),
            include_file_stats: include_file_stats.unwrap_or(true),
            include_diffs: include_diffs.unwrap_or(false),
            paths: paths.clone(),
            diff_pattern: diff_pattern.clone(),
        },
    )?;

    let mut entries = parse_git_log(
        &log_output,
        include_file_stats.unwrap_or(true),
        include_diffs.unwrap_or(false),
        remote_url.as_deref(),
    );

    let mut total_insertions = 0;
    let mut total_deletions = 0;
    let mut merge_commits = 0;

    for entry in &entries {
        total_insertions += entry.insertions;
        total_deletions += entry.deletions;
        if entry.is_merge {
            merge_commits += 1;
        }
    }

    let captured_at = current_time_millis();
    for entry in &mut entries {
        entry.captured_at = Some(captured_at);
    }

    let storage_entries = entries.clone();
    let database_path = persist_timeline_entries(
        &absolute_root,
        database_name.as_deref(),
        &branch_name,
        captured_at,
        &storage_entries,
    )?;

    let response_entries = transform_entries_for_response(entries);

    let normalized_paths = paths.as_ref().map(|values| {
        values
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
    });

    let normalized_diff_pattern = diff_pattern
        .as_ref()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .map(|pattern| pattern.to_string());

    Ok(RepositoryTimelineResponse {
        repository_root: repo_root,
        branch: branch_name,
        limit: limit.unwrap_or(20),
        since,
        include_merges: include_merges.unwrap_or(true),
        include_file_stats: include_file_stats.unwrap_or(true),
        include_diffs: include_diffs.unwrap_or(false),
        paths: normalized_paths,
        diff_pattern: normalized_diff_pattern,
        total_commits: response_entries.len(),
        merge_commits,
        total_insertions,
        total_deletions,
        entries: response_entries,
        remote_url,
        database_path,
    })
}

fn fetch_repository_timeline_entry(
    params: RepositoryTimelineEntryLookupParams,
) -> Result<RepositoryTimelineEntryLookupResponse, RepositoryTimelineError> {
    let RepositoryTimelineEntryLookupParams {
        root,
        database_name,
        commit_sha,
    } = params;

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;
    let db_path = resolve_database_path(&absolute_root, database_name.as_deref());
    let db_path_string = db_path.to_string_lossy().to_string();

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
        |error| RepositoryTimelineError::Database {
            path: db_path_string.clone(),
            source: error,
        },
    )?;

    let mut stmt = conn
        .prepare(
            "SELECT branch, captured_at, payload, diff FROM repository_timeline_entries WHERE commit_sha = ?1",
        )
        .map_err(|error| RepositoryTimelineError::Database {
            path: db_path_string.clone(),
            source: error,
        })?;

    let result = stmt.query_row(params![&commit_sha], |row| {
        let branch: String = row.get(0)?;
        let captured_at: i64 = row.get(1)?;
        let payload: String = row.get(2)?;
        let diff: Option<String> = row.get(3)?;
        Ok((branch, captured_at, payload, diff))
    });

    let (_branch, captured_at, payload, diff) = match result {
        Ok(values) => values,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(RepositoryTimelineError::EntryNotFound {
                commit_sha,
                path: db_path_string,
            })
        }
        Err(error) => {
            return Err(RepositoryTimelineError::Database {
                path: db_path_string,
                source: error,
            })
        }
    };

    let mut entry: RepositoryTimelineEntry = serde_json::from_str(&payload)?;
    let diff_preview = diff.as_ref().map(|value| build_diff_preview(value));
    let diff_pointer = diff
        .as_ref()
        .map(|_| format!("stored://repository_timeline/{}", commit_sha));

    entry.diff = diff_pointer;
    entry.diff_preview = diff_preview;
    entry.diff_pointer = diff.as_ref().map(|_| commit_sha.clone());
    entry.captured_at = Some(captured_at);

    Ok(RepositoryTimelineEntryLookupResponse {
        database_path: db_path_string,
        entry,
        diff,
    })
}

fn resolve_root(root: &str) -> Result<PathBuf, RepositoryTimelineError> {
    let candidate = PathBuf::from(root);
    if candidate.is_absolute() {
        return Ok(candidate);
    }

    let cwd = std::env::current_dir().map_err(|source| RepositoryTimelineError::InvalidRoot {
        path: root.to_string(),
        source,
    })?;
    Ok(cwd.join(candidate))
}

fn verify_git_repository(root: &PathBuf) -> Result<String, RepositoryTimelineError> {
    let status =
        std::fs::metadata(root).map_err(|source| RepositoryTimelineError::InvalidRoot {
            path: root.to_string_lossy().to_string(),
            source,
        })?;

    if !status.is_dir() {
        return Err(RepositoryTimelineError::InvalidRoot {
            path: root.to_string_lossy().to_string(),
            source: std::io::Error::other("path is not a directory"),
        });
    }

    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(root)
        .output()
        .map_err(|error| RepositoryTimelineError::Git(error.to_string()))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(RepositoryTimelineError::NotAGitRepository {
            path: root.to_string_lossy().to_string(),
            message,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

struct GitLogOptions<'a> {
    branch: &'a str,
    limit: u32,
    since: Option<&'a str>,
    include_merges: bool,
    include_file_stats: bool,
    include_diffs: bool,
    paths: Option<Vec<String>>,
    diff_pattern: Option<String>,
}

fn run_git_log(
    repo_root: &str,
    options: GitLogOptions<'_>,
) -> Result<String, RepositoryTimelineError> {
    let GitLogOptions {
        branch,
        limit,
        since,
        include_merges,
        include_file_stats,
        include_diffs,
        paths,
        diff_pattern,
    } = options;

    let mut args = Vec::new();
    args.push("log".to_string());
    args.push("--no-color".to_string());
    args.push("--date-order".to_string());
    args.push(format!("--max-count={}", limit.max(1)));

    if include_diffs {
        args.push("--patch".to_string());
    }

    let format_parts = ["%H", "%an", "%ae", "%aI", "%cn", "%ce", "%cI", "%s", "%P"];
    args.push(format!(
        "--format={}{}",
        GIT_LOG_RECORD_SEPARATOR,
        format_parts.join(GIT_LOG_FIELD_SEPARATOR)
    ));

    if include_file_stats {
        args.push("--numstat".to_string());
    }

    if !include_merges {
        args.push("--no-merges".to_string());
    }

    if let Some(pattern) = diff_pattern
        .as_ref()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
    {
        args.push("-G".to_string());
        args.push(pattern.to_string());
    }

    if let Some(since) = since.map(normalize_since_input) {
        args.push(format!("--since={since}"));
    }

    args.push(branch.to_string());

    if let Some(path_filters) = paths {
        let filtered: Vec<String> = path_filters
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        if !filtered.is_empty() {
            args.push("--".to_string());
            args.extend(filtered);
        }
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output()
        .map_err(|error| RepositoryTimelineError::Git(error.to_string()))?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(RepositoryTimelineError::Git(message));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_git_log(
    output: &str,
    include_file_stats: bool,
    include_diffs: bool,
    remote_url: Option<&str>,
) -> Vec<RepositoryTimelineEntry> {
    let mut entries = Vec::new();

    for raw_record in output.split(GIT_LOG_RECORD_SEPARATOR) {
        let record = raw_record.trim();
        if record.is_empty() {
            continue;
        }

        let mut lines = record.lines();
        let header_line = match lines.next() {
            Some(line) => line,
            None => continue,
        };

        let fields: Vec<&str> = header_line.split(GIT_LOG_FIELD_SEPARATOR).collect();
        if fields.len() < 9 {
            continue;
        }

        let sha = fields[0].to_string();
        let author_name = fields[1].to_string();
        let author_email = fields[2].to_string();
        let author_date = fields[3].to_string();
        let committer_name = fields[4].to_string();
        let committer_email = fields[5].to_string();
        let committer_date = fields[6].to_string();
        let subject = fields[7].to_string();
        let parents: Vec<String> = fields[8]
            .split(' ')
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
            .collect();
        let is_merge = parents.len() > 1;

        let stat_lines: Vec<&str> = lines.collect();

        let mut insertions = 0i64;
        let mut deletions = 0i64;
        let mut file_changes = Vec::new();
        let mut diff_start_index: Option<usize> = None;

        if include_file_stats {
            for (index, raw_line) in stat_lines.iter().enumerate() {
                if include_diffs && raw_line.starts_with("diff --git ") {
                    diff_start_index = Some(index);
                    break;
                }

                let trimmed = raw_line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let parts: Vec<&str> = trimmed.split('\t').collect();
                if parts.len() < 3 {
                    continue;
                }

                let insert_part = parts[0];
                let delete_part = parts[1];
                let path = parts[2];

                let parsed_insertions = parse_stat_value(insert_part);
                let parsed_deletions = parse_stat_value(delete_part);

                if let Some(value) = parsed_insertions {
                    insertions += value;
                }
                if let Some(value) = parsed_deletions {
                    deletions += value;
                }

                let net = match (parsed_insertions, parsed_deletions) {
                    (Some(i), Some(d)) => Some(i - d),
                    _ => None,
                };

                file_changes.push(RepositoryTimelineFileChange {
                    path: path.to_string(),
                    insertions: parsed_insertions,
                    deletions: parsed_deletions,
                    net,
                });
            }
        }

        let diff = if include_diffs {
            let start_index = diff_start_index.or_else(|| {
                stat_lines
                    .iter()
                    .position(|line| line.starts_with("diff --git "))
            });
            start_index.and_then(|index| {
                let patch_text = stat_lines[index..].join("\n").trim().to_string();
                if patch_text.is_empty() {
                    None
                } else {
                    Some(patch_text)
                }
            })
        } else {
            None
        };

        let top_files = if include_file_stats {
            to_top_files(&file_changes, 3)
        } else {
            Vec::new()
        };

        let directory_churn = if include_file_stats {
            aggregate_directory_churn(&file_changes, 5)
        } else {
            Vec::new()
        };

        let diff_summary = RepositoryTimelineDiffSummary {
            files_changed: file_changes.len(),
            insertions,
            deletions,
            net: insertions - deletions,
        };

        let pull_request_number = parse_pull_request_number(&subject);
        let pull_request_url = build_pull_request_url(remote_url, pull_request_number);

        let mut entry = RepositoryTimelineEntry {
            sha: sha.clone(),
            subject: subject.clone(),
            summary: subject.clone(),
            author: TimelineIdentity {
                name: author_name,
                email: author_email,
            },
            author_date,
            committer: TimelineIdentity {
                name: committer_name,
                email: committer_email,
            },
            committer_date,
            parents,
            is_merge,
            pull_request_number,
            files_changed: file_changes.len(),
            insertions,
            deletions,
            file_changes,
            diff,
            diff_preview: None,
            diff_pointer: None,
            top_files,
            directory_churn,
            diff_summary,
            highlights: Vec::new(),
            pull_request_url,
            captured_at: None,
        };

        entry.highlights = build_highlights(&entry);

        entries.push(entry);
    }

    entries
}

fn parse_stat_value(value: &str) -> Option<i64> {
    if value == "-" {
        None
    } else {
        value.trim().parse::<i64>().ok()
    }
}

fn to_top_files(
    changes: &[RepositoryTimelineFileChange],
    limit: usize,
) -> Vec<RepositoryTimelineTopFile> {
    let mut files: Vec<RepositoryTimelineTopFile> = changes
        .iter()
        .map(|change| {
            let insertions = change.insertions.unwrap_or(0);
            let deletions = change.deletions.unwrap_or(0);
            RepositoryTimelineTopFile {
                path: change.path.clone(),
                insertions,
                deletions,
                net: insertions - deletions,
            }
        })
        .collect();

    files.sort_by(|a, b| {
        let magnitude_a = a.insertions.abs() + a.deletions.abs();
        let magnitude_b = b.insertions.abs() + b.deletions.abs();
        magnitude_b
            .cmp(&magnitude_a)
            .then_with(|| a.path.cmp(&b.path))
    });

    files.truncate(limit);
    files
}

fn aggregate_directory_churn(
    changes: &[RepositoryTimelineFileChange],
    limit: usize,
) -> Vec<RepositoryTimelineDirectoryChurn> {
    let mut map: HashMap<String, (i64, i64, HashSet<String>)> = HashMap::new();

    for change in changes {
        let path = change.path.as_str();
        let directory = match path.rfind('/') {
            Some(index) => &path[..index],
            None => ".",
        };

        let entry = map
            .entry(directory.to_string())
            .or_insert_with(|| (0, 0, HashSet::new()));
        entry.0 += change.insertions.unwrap_or(0);
        entry.1 += change.deletions.unwrap_or(0);
        entry.2.insert(change.path.clone());
    }

    let mut entries: Vec<RepositoryTimelineDirectoryChurn> = map
        .into_iter()
        .map(
            |(path, (insertions, deletions, files))| RepositoryTimelineDirectoryChurn {
                path,
                insertions,
                deletions,
                net: insertions - deletions,
                files_changed: files.len(),
            },
        )
        .collect();

    entries.sort_by(|a, b| {
        let magnitude_a = a.insertions.abs() + a.deletions.abs();
        let magnitude_b = b.insertions.abs() + b.deletions.abs();
        magnitude_b
            .cmp(&magnitude_a)
            .then_with(|| a.path.cmp(&b.path))
    });

    entries.truncate(limit);
    entries
}

fn build_highlights(entry: &RepositoryTimelineEntry) -> Vec<String> {
    let mut highlights = Vec::new();

    match (entry.pull_request_number, &entry.pull_request_url) {
        (Some(number), Some(url)) => highlights.push(format!("PR #{number} · {url}")),
        (Some(number), None) => highlights.push(format!("PR #{number}")),
        _ => {}
    }

    if entry.is_merge {
        highlights.push("Merge commit".to_string());
    }

    let diff = &entry.diff_summary;
    highlights.push(format!(
        "Diff +{}/-{} across {} file{}",
        diff.insertions,
        diff.deletions,
        diff.files_changed,
        if diff.files_changed == 1 { "" } else { "s" }
    ));

    for file in &entry.top_files {
        highlights.push(format!(
            "{}: +{}/-{}",
            file.path, file.insertions, file.deletions
        ));
    }

    if let Some(dir) = entry.directory_churn.first() {
        highlights.push(format!(
            "{} hotspot: +{}/-{} ({} file{})",
            dir.path,
            dir.insertions,
            dir.deletions,
            dir.files_changed,
            if dir.files_changed == 1 { "" } else { "s" }
        ));
    }

    highlights
}

fn build_diff_preview(diff: &str) -> String {
    let mut preview = String::new();
    let mut char_count = 0usize;
    let mut truncated = false;

    for (index, line) in diff.lines().enumerate() {
        if index >= DIFF_PREVIEW_MAX_LINES {
            truncated = true;
            break;
        }

        if index > 0 {
            if char_count + 1 >= DIFF_PREVIEW_MAX_CHARS {
                truncated = true;
                break;
            }
            preview.push('\n');
            char_count += 1;
        }

        for ch in line.chars() {
            if char_count >= DIFF_PREVIEW_MAX_CHARS {
                truncated = true;
                break;
            }
            preview.push(ch);
            char_count += 1;
        }

        if truncated {
            break;
        }
    }

    if truncated {
        if !preview.ends_with('\n') {
            preview.push('\n');
        }
        preview.push('…');
    }

    preview
}

fn transform_entries_for_response(
    entries: Vec<RepositoryTimelineEntry>,
) -> Vec<RepositoryTimelineEntry> {
    entries
        .into_iter()
        .map(|mut entry| {
            if let Some(raw_diff) = entry.diff.take() {
                let pointer = entry.sha.clone();
                entry.diff_preview = Some(build_diff_preview(&raw_diff));
                entry.diff_pointer = Some(pointer.clone());
                entry.diff = Some(format!("stored://repository_timeline/{pointer}"));
            }
            entry
        })
        .collect()
}

fn persist_timeline_entries(
    root: &Path,
    database_name: Option<&str>,
    branch: &str,
    captured_at: i64,
    entries: &[RepositoryTimelineEntry],
) -> Result<Option<String>, RepositoryTimelineError> {
    let db_path = resolve_database_path(root, database_name);
    let db_path_string = db_path.to_string_lossy().to_string();

    if entries.is_empty() {
        return Ok(Some(db_path_string));
    }

    let mut conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|error| RepositoryTimelineError::Database {
        path: db_path_string.clone(),
        source: error,
    })?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS repository_timeline_entries (
            commit_sha TEXT PRIMARY KEY,
            branch TEXT NOT NULL,
            captured_at INTEGER NOT NULL,
            payload TEXT NOT NULL,
            diff TEXT
        )",
        [],
    )
    .map_err(|error| RepositoryTimelineError::Database {
        path: db_path_string.clone(),
        source: error,
    })?;

    let tx = conn
        .transaction()
        .map_err(|error| RepositoryTimelineError::Database {
            path: db_path_string.clone(),
            source: error,
        })?;

    let mut stmt = tx
        .prepare(
            "INSERT INTO repository_timeline_entries (commit_sha, branch, captured_at, payload, diff)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(commit_sha) DO UPDATE SET
                 branch = excluded.branch,
                 captured_at = excluded.captured_at,
                 payload = excluded.payload,
                 diff = excluded.diff",
        )
        .map_err(|error| RepositoryTimelineError::Database {
            path: db_path_string.clone(),
            source: error,
        })?;

    for entry in entries {
        let mut payload_entry = entry.clone();
        payload_entry.diff = None;
        payload_entry.diff_preview = None;
        payload_entry.diff_pointer = None;
        payload_entry.captured_at = Some(captured_at);

        let payload_json = serde_json::to_string(&payload_entry)?;
        let diff_value = entry.diff.as_deref();

        stmt.execute(params![
            entry.sha,
            branch,
            captured_at,
            payload_json,
            diff_value
        ])
        .map_err(|error| RepositoryTimelineError::Database {
            path: db_path_string.clone(),
            source: error,
        })?;
    }

    drop(stmt);

    tx.commit()
        .map_err(|error| RepositoryTimelineError::Database {
            path: db_path_string.clone(),
            source: error,
        })?;

    Ok(Some(db_path_string))
}

fn resolve_database_path(root: &Path, database_name: Option<&str>) -> PathBuf {
    let filename = database_name.unwrap_or(DEFAULT_DB_FILENAME);
    root.join(filename)
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn normalize_remote_url(raw: Option<String>) -> Option<String> {
    let value = raw?.trim().to_string();
    if value.is_empty() {
        return None;
    }

    if value.starts_with("http://") || value.starts_with("https://") {
        if let Ok(mut url) = url::Url::parse(&value) {
            url.set_username("").ok();
            url.set_password(None).ok();
            url.set_query(None);
            url.set_fragment(None);
            let mut path = url.path().to_string();
            if path.ends_with(".git") {
                path.truncate(path.len() - 4);
            }
            url.set_path(&path);
            return Some(url.to_string());
        }
    }

    if let Some((host, path)) = value.strip_prefix("git@").and_then(|s| s.split_once(':')) {
        let mut path = path.trim().to_string();
        if path.ends_with(".git") {
            path.truncate(path.len() - 4);
        }
        return Some(format!("https://{host}/{}", path.trim_start_matches('/')));
    }

    if let Some(stripped) = value.strip_prefix("ssh://") {
        if let Some((_, rest)) = stripped.split_once('@') {
            if let Some((host, path)) = rest.split_once('/') {
                let mut path = path.trim().to_string();
                if path.ends_with(".git") {
                    path.truncate(path.len() - 4);
                }
                return Some(format!("https://{host}/{}", path.trim_start_matches('/')));
            }
        }
    }

    None
}

fn build_pull_request_url(remote_url: Option<&str>, pr_number: Option<i64>) -> Option<String> {
    match (remote_url, pr_number) {
        (Some(url), Some(number)) => Some(format!("{}/pull/{}", url.trim_end_matches('/'), number)),
        _ => None,
    }
}

fn parse_pull_request_number(subject: &str) -> Option<i64> {
    for regex in PR_PATTERNS.iter() {
        if let Some(captures) = regex.captures(subject) {
            if let Some(matched) = captures.get(1) {
                if let Ok(value) = matched.as_str().parse::<i64>() {
                    return Some(value);
                }
            }
        }
    }

    None
}

fn normalize_since_input(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(captures) = RELATIVE_SINCE_PATTERN.captures(trimmed) {
        let amount = captures.get(1).unwrap().as_str();
        let unit = captures.get(2).unwrap().as_str().to_lowercase();
        return match unit.as_str() {
            "d" => format!("{amount}.days"),
            "w" => format!("{amount}.weeks"),
            "m" => format!("{amount}.months"),
            "y" => format!("{amount}.years"),
            _ => trimmed.to_string(),
        };
    }
    trimmed.to_string()
}

fn resolve_remote_url(repo_root: &str) -> Result<Option<String>, RepositoryTimelineError> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_root)
        .output()
        .map_err(|error| RepositoryTimelineError::Git(error.to_string()))?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}
