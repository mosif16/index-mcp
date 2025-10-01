use std::fs;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::task::JoinError;

use crate::index_status::DEFAULT_DB_FILENAME;

const DEFAULT_SNIPPET_LIMIT: usize = 3;
const MAX_SNIPPET_LIMIT: usize = 10;
const DEFAULT_NEIGHBOR_LIMIT: usize = 12;
const MAX_NEIGHBOR_LIMIT: usize = 50;
const DEFAULT_TOKEN_BUDGET: usize = 3_000;

#[derive(Debug, Deserialize, JsonSchema)]
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
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SymbolSelector {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
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
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleFileMetadata {
    pub path: String,
    pub size: i64,
    pub modified: i64,
    pub hash: String,
    pub last_indexed_at: i64,
    #[serde(skip_serializing)]
    pub content: Option<String>,
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

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleEdgeNeighbor {
    pub id: String,
    pub r#type: String,
    pub direction: NeighborDirection,
    pub metadata: Option<Value>,
    pub neighbor: NeighborNode,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NeighborNode {
    pub id: String,
    pub path: Option<String>,
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleSnippet {
    pub source: SnippetSource,
    pub chunk_index: Option<i32>,
    pub content: String,
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BundleIngestionSummary {
    pub id: String,
    pub finished_at: i64,
    pub duration_ms: i64,
    pub file_count: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
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

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum QuickLinkType {
    File,
    RelatedSymbol,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
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
    } = params;

    let root_path = resolve_root(root.unwrap_or_else(|| "./".to_string()))?;
    let db_path = root_path.join(database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string()));
    let db_path_string = db_path.to_string_lossy().to_string();

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(ContextBundleError::Sqlite)?;

    let target_file = normalize_file(&file);

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

    let snippets = load_snippets(&conn, &target_file, max_snippets);
    let trimmed_snippets = trim_snippets_to_budget(snippets, &definitions, budget_tokens);

    let ingestion = load_latest_ingestion(&conn)?;
    let warnings = gather_warnings(&definitions, file_content.as_deref());
    let quick_links = build_quick_links(
        &target_file,
        &definitions,
        &related,
        focus_definition.as_ref(),
    );

    Ok(ContextBundleResponse {
        database_path: db_path_string,
        file: BundleFileMetadata {
            path: target_file,
            size: file_record.size,
            modified: file_record.modified,
            hash: file_record.hash,
            last_indexed_at: file_record.last_indexed_at,
            content: file_record.content.take(),
        },
        definitions,
        focus_definition,
        related,
        snippets: trimmed_snippets,
        latest_ingestion: ingestion,
        warnings,
        quick_links,
    })
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
        "SELECT id, type, source_id, target_id, metadata FROM code_graph_edges WHERE source_id = ?1 OR target_id = ?1",
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
                ))
            })
            .ok();

        if let Some(rows) = rows {
            for row in rows.flatten() {
                let (edge_id, edge_type, source_id, target_id, metadata_raw) = row;
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
            "SELECT id, path, kind, name, signature, metadata FROM code_graph_nodes WHERE id = ?1",
        )
        .ok()?;
    stmt.query_row(params![node_id], |row| {
        let metadata_raw: Option<String> = row.get(5)?;
        Ok(NeighborNode {
            id: row.get(0)?,
            path: row.get(1)?,
            kind: row.get(2)?,
            name: row.get(3)?,
            signature: row.get(4)?,
            metadata: metadata_raw
                .as_deref()
                .and_then(|payload| serde_json::from_str::<Value>(payload).ok()),
        })
    })
    .ok()
}

fn load_snippets(conn: &Connection, path: &str, max_snippets: usize) -> Vec<BundleSnippet> {
    let mut stmt = match conn.prepare(
        "SELECT chunk_index, content, byte_start, byte_end, line_start, line_end FROM file_chunks WHERE path = ?1 ORDER BY chunk_index ASC LIMIT ?2",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(params![path, max_snippets as i64], |row| {
        Ok(BundleSnippet {
            source: SnippetSource::Chunk,
            chunk_index: Some(row.get(0)?),
            content: row.get(1)?,
            byte_start: row.get(2)?,
            byte_end: row.get(3)?,
            line_start: row.get(4)?,
            line_end: row.get(5)?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

fn trim_snippets_to_budget(
    snippets: Vec<BundleSnippet>,
    definitions: &[BundleDefinition],
    budget_tokens: usize,
) -> Vec<BundleSnippet> {
    if snippets.is_empty() {
        return snippets;
    }

    let mut used_tokens = 0usize;
    for definition in definitions {
        used_tokens += estimate_tokens(&definition.name);
        if let Some(signature) = &definition.signature {
            used_tokens += estimate_tokens(signature);
        }
        if let Some(docstring) = &definition.docstring {
            used_tokens += estimate_tokens(docstring);
        }
    }

    let mut trimmed = Vec::new();
    for snippet in snippets {
        let snippet_tokens = estimate_tokens(&snippet.content);
        if used_tokens + snippet_tokens <= budget_tokens {
            used_tokens += snippet_tokens;
            trimmed.push(snippet);
        } else {
            let remaining = budget_tokens.saturating_sub(used_tokens);
            if remaining > 100 {
                let chars = remaining.saturating_mul(4);
                let truncated = snippet.content.chars().take(chars).collect::<String>();
                trimmed.push(BundleSnippet {
                    content: format!("{}\n... (truncated due to budget)", truncated),
                    ..snippet
                });
            }
            break;
        }
    }
    trimmed
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
            path: neighbor.neighbor.path.clone(),
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
