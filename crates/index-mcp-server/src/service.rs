use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bundle::{
    context_bundle, ContextBundleError, ContextBundleParams, ContextBundleResponse, SymbolSelector,
};
use crate::environment::{collect_model_cache_info, ModelCacheInfo};
use crate::git_timeline::{
    repository_timeline, RepositoryTimelineError, RepositoryTimelineParams,
    RepositoryTimelineResponse,
};
use crate::graph_neighbors::{
    graph_neighbors, GraphNeighborDirection, GraphNeighborsError, GraphNeighborsParams,
    GraphNeighborsResponse, GraphNodeSelector,
};
use crate::index_status::{
    get_index_status, IndexStatusError, IndexStatusParams, IndexStatusResponse,
};
use crate::ingest::{ingest_codebase, IngestError, IngestParams, IngestResponse};
use crate::remote_proxy::RemoteProxyRegistry;
use crate::search::{
    semantic_search, summarize_semantic_search, SemanticSearchError, SemanticSearchParams,
    SemanticSearchResponse,
};
use once_cell::sync::Lazy;
use rustc_version_runtime::version as rustc_version;
use std::process;

use rmcp::{
    handler::server::{
        router::prompt::PromptRouter, router::tool::ToolRouter, wrapper::Parameters,
    },
    model::{
        CallToolResult, Content, GetPromptRequestParam, GetPromptResult, Implementation,
        ListPromptsResult, PaginatedRequestParam, PromptMessage, PromptMessageRole,
        ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CodeLookupParams {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    database_name: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    symbol: Option<CodeLookupSymbol>,
    #[serde(default)]
    node: Option<CodeLookupGraphNode>,
    #[serde(default)]
    direction: Option<GraphNeighborDirection>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    max_snippets: Option<u32>,
    #[serde(default)]
    max_neighbors: Option<u32>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CodeLookupSymbol {
    name: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    path: Option<Option<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CodeLookupGraphNode {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    path: Option<Option<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CodeLookupResponse {
    mode: String,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_result: Option<SemanticSearchResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    graph_result: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SemanticSearchRequest {
    #[serde(default)]
    root: Option<String>,
    query: String,
    #[serde(default)]
    database_name: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EmptyParams {}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct IndexingGuidanceToolResponse {
    guidance: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InfoResponse {
    name: String,
    version: String,
    description: Option<String>,
    instructions: String,
    native_module: NativeModuleStatus,
    environment: InfoEnvironment,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct NativeModuleStatus {
    status: NativeModuleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum NativeModuleState {
    Ready,
    Unavailable,
    Error,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InfoEnvironment {
    node_version: String,
    platform: String,
    cwd: String,
    pid: u32,
    model_cache: ModelCacheInfo,
}

/// Textual instructions shared with MCP clients.
static SERVER_INSTRUCTIONS: Lazy<String> = Lazy::new(|| {
    format!(
        "Tools available from {} v{}: code_lookup (routes to semantic search, context bundles, or graph neighbors), ingest_codebase (refresh the SQLite index; pass autoEvict/maxDatabaseSizeBytes to prune cold data), index_status (verify index freshness), semantic_search (direct embedding-powered retrieval with hotness tracking), context_bundle (assemble focused file context trimmed to the budgetTokens limit), graph_neighbors (inspect structural relationships), repository_timeline (summarize recent git commits and file churn), indexing_guidance_tool (return these reminders as a tool), indexing_guidance (prompt form of the guidance), and info (runtime diagnostics). Preferred workflow: (1) on a new checkout or after edits, run ingest_codebase with the workspace root or specific changed paths; (2) call index_status whenever you are unsure the index is current; (3) reach for code_lookup first—use query=\"...\" for discovery, file=\"...\" plus optional symbol for file context, and mode=\"graph\" when you need relationship exploration. Call semantic_search when you want raw retrieval snippets, context_bundle for a structured file packet, or graph_neighbors to expand through the GraphRAG index; each call updates hit counters so eviction keeps frequently used data resident. Set the INDEX_MCP_BUDGET_TOKENS environment variable or pass budgetTokens/context_bundle arguments to cap tokens and avoid overloading downstream LLM context, and enable autoEvict on ingest when you need the server to trim the database automatically. Use repository_timeline to review recent commits, merges, and churn so plan-of-action agents can factor fresh changes into their reasoning; diffs ship by default so code changes stay visible. If ingest_codebase reports \"UNIQUE constraint failed: code_graph_nodes...\", rerun it with graph.enabled set to false while the duplicate-node bug is addressed, and always respect ignore files so repeated ingests stay clean.",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    )
});
const INDEXING_GUIDANCE_PROMPT_TEXT: &str = "Tools: code_lookup (routes to semantic search, context bundles, or graph neighbors), ingest_codebase (refresh the SQLite index; enable autoEvict/maxDatabaseSizeBytes to prune unused chunks), index_status (check freshness), semantic_search (direct embedding-powered retrieval that updates hotness), context_bundle (structured bundle trimmed to budgetTokens or INDEX_MCP_BUDGET_TOKENS), graph_neighbors (relationship explorer), indexing_guidance_tool (tool-form reminders), indexing_guidance (prompt version), repository_timeline (summarize recent git commits), and info (runtime diagnostics). Workflow: run ingest_codebase on a new checkout or after edits while respecting .gitignore, call index_status when freshness is uncertain, then reach for code_lookup—use query=\"...\" for discovery, file=\"...\" plus optional symbol for file context, and mode=\"graph\" for relationship exploration. If ingest_codebase hits a \"UNIQUE constraint failed: code_graph_nodes...\" error, rerun it with graph.enabled=false until the duplicate-node fix lands. Invoke the specialist tools directly when you need their richer metadata, and tune budgetTokens to keep downstream LLM context under control.";

/// Primary server state for the Rust MCP implementation.
#[derive(Clone)]
pub struct IndexMcpService {
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    remote_registry: RemoteProxyRegistry,
}

impl IndexMcpService {
    pub async fn new() -> Result<Self> {
        let mut tool_router = Self::tool_router();
        let prompt_router = Self::prompt_router();
        let remote_registry = RemoteProxyRegistry::initialize().await;
        for descriptor in remote_registry.tool_descriptors().await {
            let proxy = descriptor.proxy.clone();
            let remote_name = descriptor.remote_name.clone();
            let tool_def = descriptor.tool.clone();
            let route =
                rmcp::handler::server::tool::ToolRoute::new_dyn(tool_def, move |mut context| {
                    let proxy = proxy.clone();
                    let remote_name = remote_name.clone();
                    Box::pin(async move {
                        let arguments = context.arguments.take().unwrap_or_default();
                        proxy.call_tool(&remote_name, arguments).await
                    })
                });
            tool_router.add_route(route);
        }

        Ok(Self {
            tool_router,
            prompt_router,
            remote_registry,
        })
    }
}

#[rmcp::prompt_router]
impl IndexMcpService {
    #[rmcp::prompt(
        name = "indexing_guidance",
        description = "When to run ingest_codebase to keep the index synchronized."
    )]
    fn indexing_guidance_prompt(&self) -> GetPromptResult {
        GetPromptResult {
            description: Some(
                "When to run ingest_codebase to keep the index synchronized.".to_string(),
            ),
            messages: vec![PromptMessage::new_text(
                PromptMessageRole::Assistant,
                INDEXING_GUIDANCE_PROMPT_TEXT,
            )],
        }
    }
}

#[tool_router]
impl IndexMcpService {
    #[tool(
        name = "ingest_codebase",
        description = "Walk a codebase and refresh the SQLite index."
    )]
    async fn ingest_codebase(
        &self,
        Parameters(params): Parameters<IngestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let response = ingest_codebase(params)
            .await
            .map_err(convert_ingest_error)?;

        build_ingest_result(response)
    }

    #[tool(
        name = "semantic_search",
        description = "Search indexed chunks using embeddings."
    )]
    async fn semantic_search_tool(
        &self,
        Parameters(params): Parameters<SemanticSearchRequest>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let search_params = SemanticSearchParams {
            root: params.root,
            query: params.query,
            database_name: params.database_name,
            limit: params.limit,
            model: params.model,
        };

        let response = semantic_search(search_params)
            .await
            .map_err(convert_semantic_search_error)?;

        build_semantic_search_result(response)
    }

    #[tool(
        name = "context_bundle",
        description = "Return file-level definitions, snippets, and related graph neighbors."
    )]
    async fn context_bundle_tool(
        &self,
        Parameters(params): Parameters<ContextBundleParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let response = context_bundle(params)
            .await
            .map_err(convert_context_bundle_error)?;

        build_context_bundle_result(response)
    }

    #[tool(
        name = "code_lookup",
        description = "Route lookups to semantic search (search mode only)."
    )]
    async fn code_lookup(
        &self,
        Parameters(params): Parameters<CodeLookupParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let CodeLookupParams {
            root,
            database_name,
            mode,
            query,
            file,
            symbol,
            node,
            direction,
            limit,
            max_snippets,
            max_neighbors,
            model,
        } = params;

        let trimmed_query = query
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty());
        let trimmed_file = file
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty());
        let has_graph_descriptor = node
            .as_ref()
            .map(|candidate| {
                candidate.id.is_some()
                    || candidate
                        .name
                        .as_ref()
                        .map(|value| !value.is_empty())
                        .unwrap_or(false)
            })
            .unwrap_or(false)
            || symbol
                .as_ref()
                .map(|candidate| !candidate.name.trim().is_empty())
                .unwrap_or(false);

        if mode.is_none()
            && trimmed_query.is_none()
            && trimmed_file.is_none()
            && !has_graph_descriptor
        {
            return Err(McpError::invalid_params(
                "Provide a query, file, or graph node to look up.",
                None,
            ));
        }

        let resolved_mode = mode.unwrap_or_else(|| {
            if trimmed_query.is_some() {
                "search".to_string()
            } else if trimmed_file.is_some() {
                "bundle".to_string()
            } else {
                "graph".to_string()
            }
        });

        match resolved_mode.as_str() {
            "search" => {
                let query = trimmed_query.ok_or_else(|| {
                    McpError::invalid_params("code_lookup search mode requires a query.", None)
                })?;

                let search_params = SemanticSearchParams {
                    root,
                    query: query.to_string(),
                    database_name,
                    limit,
                    model,
                };

                let response = semantic_search(search_params)
                    .await
                    .map_err(convert_semantic_search_error)?;

                build_code_lookup_result(resolved_mode, response)
            }
            "bundle" => {
                let file = file.ok_or_else(|| {
                    McpError::invalid_params("code_lookup bundle mode requires a file path.", None)
                })?;

                let bundle_params = ContextBundleParams {
                    root,
                    database_name,
                    file,
                    symbol: symbol.map(convert_symbol_selector),
                    max_snippets,
                    max_neighbors,
                    budget_tokens: None,
                };

                let response = context_bundle(bundle_params)
                    .await
                    .map_err(convert_context_bundle_error)?;

                build_code_lookup_bundle_response(resolved_mode, response)
            }
            "graph" => {
                let selector = determine_graph_node(node, symbol.clone(), file.clone())?;
                let graph_params = GraphNeighborsParams {
                    root,
                    database_name,
                    node: selector,
                    direction,
                    limit,
                };

                let graph_direction = direction.unwrap_or_default();
                let response = graph_neighbors(graph_params)
                    .await
                    .map_err(convert_graph_neighbors_error)?;

                build_code_lookup_graph_response(resolved_mode, graph_direction, response)
            }
            _ => Err(McpError::invalid_params(
                "Unsupported code_lookup mode. Supported modes: search, bundle, graph.",
                None,
            )),
        }
    }

    #[tool(
        name = "graph_neighbors",
        description = "Explore structural relationships captured during ingestion to support GraphRAG workflows."
    )]
    async fn graph_neighbors_tool(
        &self,
        Parameters(params): Parameters<GraphNeighborsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let direction = params.direction.unwrap_or_default();
        let response = graph_neighbors(params)
            .await
            .map_err(convert_graph_neighbors_error)?;

        build_graph_neighbors_result(response, direction)
    }

    #[tool(
        name = "index_status",
        description = "Summarize SQLite index freshness and coverage."
    )]
    async fn index_status(
        &self,
        Parameters(params): Parameters<IndexStatusParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let response = get_index_status(params)
            .await
            .map_err(convert_index_status_error)?;

        build_index_status_result(response)
    }

    #[tool(
        name = "repository_timeline",
        description = "Summarize recent git commits, merges, and file churn."
    )]
    async fn repository_timeline_tool(
        &self,
        Parameters(params): Parameters<RepositoryTimelineParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let response = repository_timeline(params)
            .await
            .map_err(convert_repository_timeline_error)?;

        build_repository_timeline_result(response)
    }

    #[tool(
        name = "info",
        description = "Report server metadata, version, environment, and model cache diagnostics."
    )]
    async fn info_tool(
        &self,
        Parameters(_params): Parameters<EmptyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        build_info_result()
    }

    #[tool(
        name = "indexing_guidance_tool",
        description = "Return indexing reminders as a tool for clients that cannot invoke prompts."
    )]
    async fn indexing_guidance_tool(
        &self,
        Parameters(_params): Parameters<EmptyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        build_indexing_guidance_tool_result()
    }
}

#[tool_handler]
#[rmcp::prompt_handler]
impl ServerHandler for IndexMcpService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(SERVER_INSTRUCTIONS.clone()),
        }
    }
}

fn convert_index_status_error(error: IndexStatusError) -> McpError {
    match error {
        IndexStatusError::InvalidRoot { path, source } => {
            McpError::invalid_params(format!("Unable to resolve root '{path}': {source}"), None)
        }
        IndexStatusError::Io { path, source } => {
            McpError::internal_error(format!("I/O failure accessing '{path}': {source}"), None)
        }
        IndexStatusError::Sqlite(source) => {
            McpError::internal_error(format!("SQLite error: {source}"), None)
        }
        IndexStatusError::Git(source) => {
            McpError::internal_error(format!("Git command failed: {source}"), None)
        }
        IndexStatusError::Join(source) => {
            McpError::internal_error(format!("Background task failed: {source}"), None)
        }
    }
}

fn convert_ingest_error(error: IngestError) -> McpError {
    match error {
        IngestError::InvalidRoot { path, source } => {
            McpError::invalid_params(format!("Unable to resolve root '{path}': {source}"), None)
        }
        IngestError::GlobPattern { pattern, source } => {
            McpError::invalid_params(format!("Invalid glob pattern '{pattern}': {source}"), None)
        }
        IngestError::GlobSet(source) => {
            McpError::invalid_params(format!("Failed to compile glob patterns: {source}"), None)
        }
        IngestError::Sqlite(source) => {
            McpError::internal_error(format!("SQLite error: {source}"), None)
        }
        IngestError::Embedding(message) => {
            McpError::internal_error(format!("Embedding failed: {message}"), None)
        }
        IngestError::Join(source) => {
            McpError::internal_error(format!("Background task failed: {source}"), None)
        }
    }
}

fn build_ingest_result(response: IngestResponse) -> Result<CallToolResult, McpError> {
    let summary = summarize_ingest(&response);
    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(format!("Failed to serialize ingest result: {error}"), None)
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn summarize_ingest(payload: &IngestResponse) -> String {
    let mut summary = format!(
        "Indexed {} file(s) at {} in {:.2}s.",
        payload.ingested_file_count,
        payload.root,
        payload.duration_ms as f64 / 1000.0
    );

    if !payload.skipped.is_empty() {
        summary.push_str(&format!(" Skipped {} file(s).", payload.skipped.len()));
    }

    if !payload.deleted_paths.is_empty() {
        summary.push_str(&format!(
            " Removed {} stale entr{}.",
            payload.deleted_paths.len(),
            if payload.deleted_paths.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }

    if let Some(evicted) = &payload.evicted {
        summary.push_str(&format!(
            " Evicted {} chunk(s) and {} node(s) to control database size.",
            evicted.evicted_chunks, evicted.evicted_nodes
        ));
    }

    summary
}

fn build_index_status_result(response: IndexStatusResponse) -> Result<CallToolResult, McpError> {
    let summary = summarize_index_status(&response);
    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(format!("Failed to serialize status: {error}"), None)
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn summarize_index_status(payload: &IndexStatusResponse) -> String {
    if !payload.database_exists {
        return format!(
            "SQLite index not found at {}. Run ingest_codebase to create it.",
            payload.database_path
        );
    }

    let mut summary = format!(
        "Database {} tracks {} file(s) and {} chunk(s).",
        payload.database_path, payload.total_files, payload.total_chunks
    );

    if let Some(latest) = &payload.latest_ingestion {
        summary.push_str(&format!(
            " Last ingest processed {} file(s) in {:.2}s.",
            latest.file_count,
            latest.duration_ms as f64 / 1000.0
        ));
    } else {
        summary.push_str(" No ingestion history recorded yet.");
    }

    if payload.is_stale {
        summary.push_str(" Index appears stale compared to the current git HEAD.");
    }

    if !payload.embedding_models.is_empty() {
        summary.push_str(&format!(
            " Embedding models: {}.",
            payload.embedding_models.join(", ")
        ));
    }

    summary
}

fn convert_semantic_search_error(error: SemanticSearchError) -> McpError {
    match error {
        SemanticSearchError::InvalidRoot { path, source } => {
            McpError::invalid_params(format!("Unable to resolve root '{path}': {source}"), None)
        }
        SemanticSearchError::Sqlite(source) => {
            McpError::internal_error(format!("SQLite error: {source}"), None)
        }
        SemanticSearchError::Embedding(message) => {
            McpError::internal_error(format!("Embedding failed: {message}"), None)
        }
        SemanticSearchError::Join(source) => {
            McpError::internal_error(format!("Background task failed: {source}"), None)
        }
        SemanticSearchError::MultipleModels { available } => McpError::invalid_params(
            format!("Multiple embedding models found ({available}). Specify the desired model."),
            None,
        ),
        SemanticSearchError::ModelNotFound { requested, available } => McpError::invalid_params(
            format!(
                "No chunks indexed with embedding model '{requested}'. Available models: {available}"
            ),
            None,
        ),
    }
}

fn build_semantic_search_result(
    response: SemanticSearchResponse,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_semantic_search(&response);
    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize semantic search result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_code_lookup_result(
    mode: String,
    search_result: SemanticSearchResponse,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_semantic_search(&search_result);
    let payload = CodeLookupResponse {
        mode,
        summary: summary.clone(),
        search_result: Some(search_result),
        bundle_result: None,
        graph_result: None,
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize code_lookup result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_code_lookup_bundle_response(
    mode: String,
    bundle: ContextBundleResponse,
) -> Result<CallToolResult, McpError> {
    let summary = format!(
        "Context bundle prepared for {} with {} definition(s) and {} snippet(s).",
        bundle.file.path,
        bundle.definitions.len(),
        bundle.snippets.len()
    );

    let payload = CodeLookupResponse {
        mode,
        summary: summary.clone(),
        search_result: None,
        bundle_result: Some(serde_json::to_value(&bundle).map_err(|error| {
            McpError::internal_error(
                format!("Failed to serialize context bundle result: {error}"),
                None,
            )
        })?),
        graph_result: None,
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize code_lookup result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_code_lookup_graph_response(
    mode: String,
    direction: GraphNeighborDirection,
    graph: GraphNeighborsResponse,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_graph_neighbors(direction, &graph);
    let payload = CodeLookupResponse {
        mode,
        summary: summary.clone(),
        search_result: None,
        bundle_result: None,
        graph_result: Some(serde_json::to_value(&graph).map_err(|error| {
            McpError::internal_error(
                format!("Failed to serialize graph neighbors result: {error}"),
                None,
            )
        })?),
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize code_lookup result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn convert_symbol_selector(symbol: CodeLookupSymbol) -> SymbolSelector {
    SymbolSelector {
        name: symbol.name,
        kind: symbol.kind,
        path: symbol.path,
    }
}

fn determine_graph_node(
    node: Option<CodeLookupGraphNode>,
    symbol: Option<CodeLookupSymbol>,
    file: Option<String>,
) -> Result<GraphNodeSelector, McpError> {
    if let Some(node) = node {
        let mut resolved_name = node.name.and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });

        if resolved_name.is_none() {
            if let Some(symbol) = symbol.as_ref() {
                let trimmed = symbol.name.trim();
                if !trimmed.is_empty() {
                    resolved_name = Some(trimmed.to_string());
                }
            }
        }

        if resolved_name.is_none() {
            if let Some(file) = file.as_ref() {
                let trimmed = file.trim();
                if !trimmed.is_empty() {
                    resolved_name = Some(trimmed.to_string());
                }
            }
        }

        if resolved_name.is_none() {
            if let Some(id) = node.id.as_ref() {
                resolved_name = Some(id.clone());
            }
        }

        let name = resolved_name.ok_or_else(|| {
            McpError::invalid_params(
                "code_lookup graph mode requires node name when id is not provided.",
                None,
            )
        })?;

        return Ok(GraphNodeSelector {
            name,
            id: node.id,
            kind: node.kind,
            path: node.path,
        });
    }

    if let Some(symbol) = symbol {
        let CodeLookupSymbol { name, kind, path } = symbol;
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return Err(McpError::invalid_params(
                "code_lookup graph mode requires node or symbol with a name.",
                None,
            ));
        }

        let mut resolved_path = path;
        if resolved_path.is_none() {
            if let Some(file) = file.as_ref() {
                let trimmed = file.trim();
                if !trimmed.is_empty() {
                    resolved_path = Some(Some(trimmed.to_string()));
                }
            }
        }

        return Ok(GraphNodeSelector {
            name: trimmed,
            id: None,
            kind,
            path: resolved_path,
        });
    }

    if let Some(file) = file {
        let trimmed = file.trim().to_string();
        if trimmed.is_empty() {
            return Err(McpError::invalid_params(
                "code_lookup graph mode requires node or symbol with a name.",
                None,
            ));
        }
        return Ok(GraphNodeSelector {
            name: trimmed.clone(),
            id: None,
            kind: None,
            path: Some(Some(trimmed)),
        });
    }

    Err(McpError::invalid_params(
        "code_lookup graph mode requires node or symbol with a name.",
        None,
    ))
}

fn build_indexing_guidance_tool_result() -> Result<CallToolResult, McpError> {
    let payload = IndexingGuidanceToolResponse {
        guidance: INDEXING_GUIDANCE_PROMPT_TEXT.to_string(),
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize indexing guidance result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![
            Content::text(
                "Indexing guidance provided. See structured guidance field for full reminders.",
            ),
            json_content,
        ],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_info_result() -> Result<CallToolResult, McpError> {
    let name = env!("CARGO_PKG_NAME").to_string();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let description = option_env!("CARGO_PKG_DESCRIPTION").map(|value| value.to_string());
    let instructions = SERVER_INSTRUCTIONS.clone();
    let native_module = NativeModuleStatus {
        status: NativeModuleState::Ready,
        message: None,
    };

    let cwd = std::env::current_dir().map_err(|error| {
        McpError::internal_error(
            format!("Unable to resolve current directory: {error}"),
            None,
        )
    })?;

    let model_cache = collect_model_cache_info();
    let environment = InfoEnvironment {
        node_version: format!("rustc {}", rustc_version()),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        cwd: cwd.display().to_string(),
        pid: process::id(),
        model_cache,
    };

    let payload = InfoResponse {
        name: name.clone(),
        version: version.clone(),
        description,
        instructions: instructions.clone(),
        native_module,
        environment,
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(format!("Failed to serialize info result: {error}"), None)
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![
            Content::text(format!("{} v{} is ready.", name, version)),
            json_content,
        ],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn convert_context_bundle_error(error: ContextBundleError) -> McpError {
    match error {
        ContextBundleError::InvalidRoot { path, source } => {
            McpError::invalid_params(format!("Unable to resolve root '{path}': {source}"), None)
        }
        ContextBundleError::Sqlite(source) => {
            McpError::internal_error(format!("SQLite error: {source}"), None)
        }
        ContextBundleError::Io { path, source } => {
            McpError::internal_error(format!("Failed to access '{path}': {source}"), None)
        }
        ContextBundleError::Join(source) => {
            McpError::internal_error(format!("Background task failed: {source}"), None)
        }
    }
}

fn convert_graph_neighbors_error(error: GraphNeighborsError) -> McpError {
    match error {
        GraphNeighborsError::InvalidRoot { path, source } => {
            McpError::invalid_params(format!("Unable to resolve root '{path}': {source}"), None)
        }
        GraphNeighborsError::DatabaseIo { path, source } => McpError::invalid_params(
            format!("Unable to access database '{path}': {source}"),
            None,
        ),
        GraphNeighborsError::Sqlite(source) => {
            McpError::internal_error(format!("SQLite error: {source}"), None)
        }
        GraphNeighborsError::NodeNotFound { descriptor } => {
            McpError::invalid_params(format!("No graph node found matching {descriptor}"), None)
        }
        GraphNeighborsError::NodeAmbiguous { descriptor } => McpError::invalid_params(
            format!("Multiple graph nodes matched {descriptor}; please specify an id."),
            None,
        ),
        GraphNeighborsError::Join(source) => {
            McpError::internal_error(format!("Background task failed: {source}"), None)
        }
    }
}

fn convert_repository_timeline_error(error: RepositoryTimelineError) -> McpError {
    match error {
        RepositoryTimelineError::InvalidRoot { path, source } => {
            McpError::invalid_params(format!("Unable to resolve root '{path}': {source}"), None)
        }
        RepositoryTimelineError::NotAGitRepository { path, message } => {
            McpError::invalid_params(format!("{path} is not a git repository: {message}"), None)
        }
        RepositoryTimelineError::Git(message) => {
            McpError::internal_error(format!("Git command failed: {message}"), None)
        }
        RepositoryTimelineError::Join(source) => {
            McpError::internal_error(format!("Background task failed: {source}"), None)
        }
    }
}

fn build_context_bundle_result(
    response: ContextBundleResponse,
) -> Result<CallToolResult, McpError> {
    let summary = format!(
        "Context bundle prepared for {} with {} definition(s) and {} snippet(s).",
        response.file.path,
        response.definitions.len(),
        response.snippets.len()
    );

    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize context bundle result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_graph_neighbors_result(
    response: GraphNeighborsResponse,
    direction: GraphNeighborDirection,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_graph_neighbors(direction, &response);
    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize graph neighbors result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn summarize_graph_neighbors(
    direction: GraphNeighborDirection,
    response: &GraphNeighborsResponse,
) -> String {
    let direction_descriptor = match direction {
        GraphNeighborDirection::Incoming => "incoming",
        GraphNeighborDirection::Outgoing => "outgoing",
        GraphNeighborDirection::Both => "incoming/outgoing",
    };

    if response.neighbors.is_empty() {
        format!(
            "Graph query found no {} neighbors for node '{}'.",
            direction_descriptor, response.node.name
        )
    } else {
        format!(
            "Graph query found {} neighbor(s) ({}) for node '{}'.",
            response.neighbors.len(),
            direction_descriptor,
            response.node.name
        )
    }
}

fn build_repository_timeline_result(
    response: RepositoryTimelineResponse,
) -> Result<CallToolResult, McpError> {
    let summary = if response.total_commits == 0 {
        let since_segment = response
            .since
            .as_ref()
            .map(|value| format!(" since {}", value))
            .unwrap_or_default();
        format!(
            "No commits matched the requested filters on {}{}.",
            response.branch, since_segment
        )
    } else {
        let commit_word = if response.total_commits == 1 {
            "commit"
        } else {
            "commits"
        };
        let since_segment = response
            .since
            .as_ref()
            .map(|value| format!(" since {}", value))
            .unwrap_or_default();
        let merge_segment = if response.merge_commits > 0 {
            format!(
                " Includes {} merge{}.",
                response.merge_commits,
                if response.merge_commits == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        format!(
            "Latest {} {}{} on {}; {} insertions / {} deletions.{}",
            response.total_commits,
            commit_word,
            since_segment,
            response.branch,
            response.total_insertions,
            response.total_deletions,
            merge_segment
        )
    };

    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize repository timeline result: {error}"),
            None,
        )
    })?;

    let json_content = Content::json(value.clone()).map_err(|error| {
        McpError::internal_error(format!("Failed to encode JSON content: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary), json_content],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}
