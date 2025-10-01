use std::path::PathBuf;

use crate::index_status::DEFAULT_DB_FILENAME;
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum GraphNeighborDirection {
    Incoming,
    Outgoing,
    Both,
}

impl Default for GraphNeighborDirection {
    fn default() -> Self {
        GraphNeighborDirection::Outgoing
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphNodeSelector {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub path: Option<Option<String>>,
    #[serde(default)]
    pub kind: Option<String>,
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphNeighborsParams {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub database_name: Option<String>,
    pub node: GraphNodeSelector,
    #[serde(default)]
    pub direction: Option<GraphNeighborDirection>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphNodeSummary {
    pub id: String,
    pub path: Option<String>,
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphNeighborEdge {
    pub id: String,
    pub r#type: String,
    pub metadata: Option<Value>,
    pub direction: NeighborDirection,
    pub neighbor: GraphNodeSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphNeighborsResponse {
    pub database_path: String,
    pub node: GraphNodeSummary,
    pub neighbors: Vec<GraphNeighborEdge>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum NeighborDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Error)]
pub enum GraphNeighborsError {
    #[error("failed to resolve workspace root '{path}': {source}")]
    InvalidRoot {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to access database '{path}': {source}")]
    DatabaseIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("graph node '{descriptor}' not found")]
    NodeNotFound { descriptor: String },
    #[error("multiple graph nodes matched descriptor '{descriptor}'; please specify an id or additional filters")]
    NodeAmbiguous { descriptor: String },
    #[error("blocking task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub async fn graph_neighbors(
    params: GraphNeighborsParams,
) -> Result<GraphNeighborsResponse, GraphNeighborsError> {
    tokio::task::spawn_blocking(move || perform_graph_neighbors(params)).await?
}

fn perform_graph_neighbors(
    params: GraphNeighborsParams,
) -> Result<GraphNeighborsResponse, GraphNeighborsError> {
    let GraphNeighborsParams {
        root,
        database_name,
        node,
        direction,
        limit,
    } = params;

    let root_param = root.unwrap_or_else(|| "./".to_string());
    let absolute_root = resolve_root(&root_param)?;

    let database_name = database_name.unwrap_or_else(|| DEFAULT_DB_FILENAME.to_string());
    let database_path = absolute_root.join(&database_name);

    if !database_path.exists() {
        return Err(GraphNeighborsError::DatabaseIo {
            path: database_path.to_string_lossy().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "database not found"),
        });
    }

    let conn = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let target_node = resolve_target_node(&conn, &node)?;

    let effective_limit = normalize_limit(limit);
    let selected_direction = direction.unwrap_or_default();
    let mut neighbors = Vec::new();

    if matches!(
        selected_direction,
        GraphNeighborDirection::Outgoing | GraphNeighborDirection::Both
    ) {
        neighbors.extend(query_neighbors(
            &conn,
            &target_node,
            NeighborDirection::Outgoing,
            effective_limit,
        )?);
    }

    if matches!(
        selected_direction,
        GraphNeighborDirection::Incoming | GraphNeighborDirection::Both
    ) {
        neighbors.extend(query_neighbors(
            &conn,
            &target_node,
            NeighborDirection::Incoming,
            effective_limit,
        )?);
    }

    Ok(GraphNeighborsResponse {
        database_path: database_path.to_string_lossy().to_string(),
        node: target_node,
        neighbors,
    })
}

fn resolve_target_node(
    conn: &Connection,
    selector: &GraphNodeSelector,
) -> Result<GraphNodeSummary, GraphNeighborsError> {
    if let Some(id) = selector.id.as_ref() {
        let mut stmt = conn.prepare(
            "SELECT id, path, kind, name, signature, metadata
             FROM code_graph_nodes
             WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| map_node_row(row))
            .map_err(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => GraphNeighborsError::NodeNotFound {
                    descriptor: format!("id={id}"),
                },
                other => GraphNeighborsError::Sqlite(other),
            })?;
        return Ok(row);
    }

    let mut query = String::from(
        "SELECT id, path, kind, name, signature, metadata
         FROM code_graph_nodes
         WHERE name = ?1",
    );
    let mut bind_values: Vec<String> = vec![selector.name.clone()];
    let mut param_index = 2;

    match selector.path {
        Some(Some(ref path)) => {
            query.push_str(&format!(" AND path = ?{param_index}"));
            bind_values.push(path.clone());
            param_index += 1;
        }
        Some(None) => query.push_str(" AND path IS NULL"),
        None => {}
    }

    if let Some(kind) = selector.kind.as_ref() {
        query.push_str(&format!(" AND kind = ?{param_index}"));
        bind_values.push(kind.clone());
    }

    query.push_str(" ORDER BY path IS NULL, path LIMIT 2");

    let mut stmt = conn.prepare(&query)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(bind_values.iter()))?;
    let mut results = Vec::new();
    while let Some(row) = rows.next()? {
        results.push(map_node_row(row)?);
    }

    if results.is_empty() {
        let descriptor = describe_selector(selector);
        return Err(GraphNeighborsError::NodeNotFound { descriptor });
    }

    if results.len() > 1 {
        let descriptor = describe_selector(selector);
        return Err(GraphNeighborsError::NodeAmbiguous { descriptor });
    }

    Ok(results.remove(0))
}

fn describe_selector(selector: &GraphNodeSelector) -> String {
    let mut descriptor = format!("name='{}'", selector.name);
    match selector.path {
        Some(Some(ref path)) => descriptor.push_str(&format!(", path='{path}'")),
        Some(None) => descriptor.push_str(", path=NULL"),
        None => {}
    }
    if let Some(kind) = selector.kind.as_ref() {
        descriptor.push_str(&format!(", kind='{kind}'"));
    }
    descriptor
}

fn query_neighbors(
    conn: &Connection,
    node: &GraphNodeSummary,
    direction: NeighborDirection,
    limit: usize,
) -> Result<Vec<GraphNeighborEdge>, GraphNeighborsError> {
    let sql = match direction {
        NeighborDirection::Outgoing => {
            "SELECT e.id, e.type, e.metadata, n.id, n.path, n.kind, n.name, n.signature, n.metadata
             FROM code_graph_edges e
             JOIN code_graph_nodes n ON n.id = e.target_id
             WHERE e.source_id = ?1
             LIMIT ?2"
        }
        NeighborDirection::Incoming => {
            "SELECT e.id, e.type, e.metadata, n.id, n.path, n.kind, n.name, n.signature, n.metadata
             FROM code_graph_edges e
             JOIN code_graph_nodes n ON n.id = e.source_id
             WHERE e.target_id = ?1
             LIMIT ?2"
        }
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![node.id, limit as i64], |row| {
        let neighbor = GraphNodeSummary {
            id: row.get(3)?,
            path: row.get::<_, Option<String>>(4)?,
            kind: row.get(5)?,
            name: row.get(6)?,
            signature: row.get(7)?,
            metadata: parse_metadata(row.get(8)?),
        };

        Ok(GraphNeighborEdge {
            id: row.get(0)?,
            r#type: row.get(1)?,
            metadata: parse_metadata(row.get(2)?),
            direction: direction.clone(),
            neighbor,
        })
    })?;

    let mut neighbors = Vec::new();
    for result in rows {
        neighbors.push(result?);
    }

    Ok(neighbors)
}

fn map_node_row(row: &rusqlite::Row<'_>) -> Result<GraphNodeSummary, rusqlite::Error> {
    Ok(GraphNodeSummary {
        id: row.get(0)?,
        path: row.get(1)?,
        kind: row.get(2)?,
        name: row.get(3)?,
        signature: row.get(4)?,
        metadata: parse_metadata(row.get(5)?),
    })
}

fn resolve_root(root: &str) -> Result<PathBuf, GraphNeighborsError> {
    let candidate = PathBuf::from(root);
    if candidate.is_absolute() {
        return Ok(candidate);
    }

    let cwd = std::env::current_dir().map_err(|source| GraphNeighborsError::InvalidRoot {
        path: root.to_string(),
        source,
    })?;
    Ok(cwd.join(candidate))
}

fn normalize_limit(limit: Option<u32>) -> usize {
    match limit {
        Some(value) if value == 0 => 0,
        Some(value) => value.min(100) as usize,
        None => 16,
    }
}

fn parse_metadata(raw: Option<String>) -> Option<Value> {
    raw.and_then(|payload| serde_json::from_str(&payload).ok())
}
