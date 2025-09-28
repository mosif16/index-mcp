use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ingest::{ingest_codebase, IngestOptions};
use crate::lookup::{fetch_file, search_files};
use crate::server::ServerState;
use crate::tool::{Tool, ToolContent, ToolContext, ToolInfo, ToolResponse};

pub fn register_all_tools(state: Arc<ServerState>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(IngestTool::new(state.clone())),
        Arc::new(CodeLookupTool::new(state.clone())),
        Arc::new(IndexStatusTool::new(state.clone())),
    ]
}

struct IngestTool {
    state: Arc<ServerState>,
}

impl IngestTool {
    fn new(state: Arc<ServerState>) -> Self {
        IngestTool { state }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestArgs {
    root: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    database_name: Option<String>,
    max_file_size_bytes: Option<u64>,
    store_file_content: Option<bool>,
}

#[async_trait]
impl Tool for IngestTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "ingest_codebase".to_string(),
            description: "Walk the repository and persist metadata/content into a SQLite database."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "root": {
                        "type": "string",
                        "description": "Path to the workspace root (defaults to server working directory)."
                    },
                    "include": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Glob patterns to include."
                    },
                    "exclude": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Additional glob patterns to exclude."
                    },
                    "databaseName": {
                        "type": "string",
                        "description": "Optional SQLite filename."
                    },
                    "maxFileSizeBytes": {
                        "type": "integer",
                        "description": "Maximum file size to ingest (bytes)."
                    },
                    "storeFileContent": {
                        "type": "boolean",
                        "description": "Whether to store UTF-8 file content in the index."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Value, ctx: ToolContext) -> Result<ToolResponse> {
        let parsed: IngestArgs =
            serde_json::from_value(args.clone()).with_context(|| "Invalid ingest arguments")?;
        let working_dir = ctx.state.working_dir.clone();
        let root_path = parsed
            .root
            .as_deref()
            .map(|value| resolve_root(&working_dir, value))
            .transpose()?
            .unwrap_or_else(|| working_dir.clone());

        let database_name = parsed
            .database_name
            .clone()
            .unwrap_or_else(|| ctx.state.default_database_name.clone());

        let mut options = IngestOptions::default();
        if let Some(include) = parsed.include {
            options.include = include;
        }
        if let Some(exclude) = parsed.exclude {
            options.exclude = exclude;
        }
        options.database_name = database_name.clone();
        options.max_file_size = parsed.max_file_size_bytes;
        if let Some(store) = parsed.store_file_content {
            options.store_content = store;
        }

        let state = self.state.clone();
        let summary = tokio::task::spawn_blocking(move || ingest_codebase(&root_path, options))
            .await
            .context("Ingest task failed")??;

        state.last_ingest.lock().replace(summary.clone());

        let text = format!(
            "Indexed {} files in {} ms ({} skipped). Database stored at {}",
            summary.run.file_count,
            summary.duration_ms,
            summary.run.skipped_count,
            summary.database_path.display()
        );

        let structured = json!({
            "databasePath": summary.database_path,
            "root": summary.run.root,
            "fileCount": summary.run.file_count,
            "skippedCount": summary.run.skipped_count,
            "totalBytes": summary.total_bytes,
            "ingestionId": summary.run.id,
            "startedAt": summary.run.started_at,
            "finishedAt": summary.run.finished_at,
            "storeFileContent": summary.run.store_content,
        });

        Ok(ToolResponse::with_structured(text, structured))
    }
}

struct CodeLookupTool {
    state: Arc<ServerState>,
}

impl CodeLookupTool {
    fn new(state: Arc<ServerState>) -> Self {
        CodeLookupTool { state }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupArgs {
    path: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for CodeLookupTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "code_lookup".to_string(),
            description: "Retrieve files or search previously ingested content.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path to retrieve."},
                    "query": {"type": "string", "description": "Substring query to search."},
                    "limit": {"type": "integer", "description": "Maximum number of search results."}
                },
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, args: Value, ctx: ToolContext) -> Result<ToolResponse> {
        let parsed: LookupArgs =
            serde_json::from_value(args).with_context(|| "Invalid lookup arguments")?;
        let last_ingest = ctx.state.last_ingest.lock().clone();
        let summary =
            last_ingest.ok_or_else(|| anyhow!("No index available. Run ingest_codebase first."))?;
        let database_path = summary.database_path.clone();

        if let Some(path) = parsed.path {
            let db = database_path.clone();
            let entry = tokio::task::spawn_blocking(move || fetch_file(&db, &path))
                .await
                .context("Lookup task failed")??;
            if let Some(file) = entry {
                let snippet = file.content.as_ref().map(|text| truncate(text, 400));
                let structured = json!({
                    "path": file.path,
                    "size": file.size,
                    "modified": file.modified,
                    "content": file.content,
                });
                let mut response = ToolResponse::with_structured(
                    format!("Retrieved {} ({} bytes)", file.path, file.size),
                    structured,
                );
                if let Some(snippet) = snippet {
                    response.content.push(ToolContent::Text { text: snippet });
                }
                return Ok(response);
            } else {
                return Ok(ToolResponse::text(format!(
                    "{} not found in the index",
                    path
                )));
            }
        }

        let query = parsed
            .query
            .ok_or_else(|| anyhow!("Either path or query is required."))?;
        let limit = parsed.limit.unwrap_or(10);
        let db = database_path.clone();
        let matches = tokio::task::spawn_blocking(move || search_files(&db, &query, limit))
            .await
            .context("Search task failed")??;

        if matches.is_empty() {
            return Ok(ToolResponse::text("No results."));
        }

        let structured = json!({
            "query": query,
            "results": matches
                .iter()
                .map(|item| json!({
                    "path": item.path,
                    "snippet": item.snippet,
                }))
                .collect::<Vec<_>>()
        });

        let summary_text = matches
            .iter()
            .map(|item| format!("- {}", item.path))
            .collect::<Vec<_>>()
            .join("\n");
        let message = format!("Top {} results:\n{}", matches.len(), summary_text);
        Ok(ToolResponse::with_structured(message, structured))
    }
}

struct IndexStatusTool {
    state: Arc<ServerState>,
}

impl IndexStatusTool {
    fn new(state: Arc<ServerState>) -> Self {
        IndexStatusTool { state }
    }
}

#[async_trait]
impl Tool for IndexStatusTool {
    fn info(&self) -> ToolInfo {
        ToolInfo {
            name: "index_status".to_string(),
            description: "Return metadata about the most recent ingest run.".to_string(),
            input_schema: json!({ "type": "object", "additionalProperties": false }),
        }
    }

    async fn call(&self, _args: Value, ctx: ToolContext) -> Result<ToolResponse> {
        let summary = ctx.state.last_ingest.lock().clone();
        if let Some(summary) = summary {
            let text = format!(
                "Last ingest indexed {} files with {} skipped at {}",
                summary.run.file_count, summary.run.skipped_count, summary.run.finished_at
            );
            let structured = json!({
                "databasePath": summary.database_path,
                "root": summary.run.root,
                "fileCount": summary.run.file_count,
                "skippedCount": summary.run.skipped_count,
                "totalBytes": summary.total_bytes,
                "finishedAt": summary.run.finished_at,
            });
            Ok(ToolResponse::with_structured(text, structured))
        } else {
            Ok(ToolResponse::text(
                "No ingest has been completed yet. Run ingest_codebase to create an index.",
            ))
        }
    }
}

fn resolve_root(base: &Path, value: &str) -> Result<PathBuf> {
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        Ok(candidate.to_path_buf())
    } else {
        Ok(base.join(candidate))
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        format!("{}…", &text[..max])
    }
}
