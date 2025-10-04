use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::sync::{Arc, RwLock};

use crate::bundle::{
    context_bundle, ContextBundleError, ContextBundleParams, ContextBundleResponse, LineRange,
    QuickLinkType, SnippetSource, SymbolSelector,
};
use crate::git_timeline::{
    repository_timeline, repository_timeline_entry_detail, RepositoryTimelineEntryLookupParams,
    RepositoryTimelineEntryLookupResponse, RepositoryTimelineError, RepositoryTimelineParams,
    RepositoryTimelineResponse,
};
use crate::index_status::{
    get_index_status, IndexStatusError, IndexStatusParams, IndexStatusResponse,
};
use crate::ingest::{ingest_codebase, warm_up_embedder, IngestError, IngestParams, IngestResponse};
use crate::remote_proxy::RemoteProxyRegistry;
use crate::search::{
    semantic_search, summarize_semantic_search, Classification, SemanticSearchError,
    SemanticSearchMatch, SemanticSearchParams, SemanticSearchResponse, SuggestedTool, SummaryMode,
};
use tracing::warn;

use rmcp::{
    handler::server::{
        router::prompt::PromptRouter, router::tool::ToolRouter, wrapper::Parameters,
    },
    model::{
        CallToolResult, Content, GetPromptRequestParam, GetPromptResult, Implementation,
        ListPromptsResult, Meta, PaginatedRequestParam, PromptMessage, PromptMessageRole,
        ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};

const DEFAULT_BUNDLE_BUDGET: usize = 2_000;
const MIN_BUNDLE_BUDGET: usize = 600;
const DEFAULT_SNIPPET_LIMIT_HINT: u32 = 2;
const DEFAULT_SEARCH_LIMIT_HINT: u32 = 6;
const SUGGESTED_RANGE_PADDING: u32 = 2;

#[derive(Debug, Clone, Default)]
struct EnvironmentSnapshot {
    cwd: Option<String>,
    bundle_budget_override: Option<usize>,
    remaining_context_tokens: Option<usize>,
    recent_hits: Vec<RecentHit>,
}

impl EnvironmentSnapshot {
    fn bundle_budget(&self) -> usize {
        let mut budget = self.bundle_budget_override.unwrap_or(DEFAULT_BUNDLE_BUDGET);
        if let Some(remaining) = self.remaining_context_tokens {
            // keep at least 40% buffer of the reported remaining window
            let safety = (remaining as f64 * 0.6).floor() as usize;
            if safety > 0 {
                budget = budget.min(safety.max(MIN_BUNDLE_BUDGET));
            }
        }
        budget.max(MIN_BUNDLE_BUDGET)
    }
}

#[derive(Debug, Clone, Default)]
struct EnvironmentState {
    inner: Arc<RwLock<EnvironmentSnapshot>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RecentHit {
    path: String,
    chunk_index: i32,
}

const RECENT_HIT_HISTORY: usize = 32;

impl EnvironmentState {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(EnvironmentSnapshot::default())),
        }
    }

    fn snapshot(&self) -> EnvironmentSnapshot {
        self.inner
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn update_from_meta(&self, meta: &Meta) {
        let Some(value) = Self::meta_to_value(meta) else {
            return;
        };
        let env_value = Self::extract_environment_value(&value);
        if env_value.is_none() && !Self::contains_environment_keys(&value) {
            return;
        }

        let mut next = self.snapshot();
        let source = env_value.unwrap_or(&value);

        if let Some(cwd) = source.get("cwd").and_then(|v| v.as_str()) {
            next.cwd = Some(cwd.trim().to_string());
        }

        if let Some(budget) = source
            .get("bundleBudgetTokens")
            .or_else(|| source.get("budgetTokens"))
            .and_then(|v| v.as_u64())
        {
            next.bundle_budget_override = Some(budget as usize);
        }

        if let Some(usage) = source.get("tokenUsage") {
            if let Some(remaining) = usage.get("remainingContextTokens").and_then(|v| v.as_u64()) {
                next.remaining_context_tokens = Some(remaining as usize);
            }
        }

        if let Some(remaining) = source
            .get("remainingContextTokens")
            .and_then(|v| v.as_u64())
        {
            next.remaining_context_tokens = Some(remaining as usize);
        }

        if let Ok(mut guard) = self.inner.write() {
            *guard = next;
        }
    }

    fn apply_ingest_defaults(&self, params: &mut IngestParams) {
        if params.root.is_none() {
            if let Some(cwd) = self.snapshot().cwd {
                params.root = Some(cwd);
            }
        }
    }

    fn apply_semantic_defaults(&self, params: &mut SemanticSearchRequest) {
        if params.root.is_none() {
            if let Some(cwd) = self.snapshot().cwd {
                params.root = Some(cwd);
            }
        }
        if params.limit.is_none() {
            params.limit = Some(DEFAULT_SEARCH_LIMIT_HINT);
        }
        if params.summary_mode.is_none() {
            params.summary_mode = Some(SummaryMode::Brief);
        }
        if params.max_context_before.is_none() {
            params.max_context_before = Some(1);
        }
        if params.max_context_after.is_none() {
            params.max_context_after = Some(1);
        }
    }

    fn apply_bundle_defaults(&self, params: &mut ContextBundleParams) {
        let snapshot = self.snapshot();
        if params.root.is_none() {
            if let Some(cwd) = snapshot.cwd.clone() {
                params.root = Some(cwd);
            }
        }
        if params.max_snippets.is_none() {
            params.max_snippets = Some(DEFAULT_SNIPPET_LIMIT_HINT);
        }
        if params.budget_tokens.is_none() {
            params.budget_tokens = Some(snapshot.bundle_budget() as u32);
        }
        if params.max_neighbors.is_none() {
            params.max_neighbors = Some(6);
        }
    }

    fn apply_code_lookup_defaults(&self, params: &mut CodeLookupParams) {
        if params.root.is_none() {
            if let Some(cwd) = self.snapshot().cwd {
                params.root = Some(cwd);
            }
        }
        if params.summary_mode.is_none() {
            params.summary_mode = Some(SummaryMode::Brief);
        }
        if params.max_context_before.is_none() {
            params.max_context_before = Some(1);
        }
        if params.max_context_after.is_none() {
            params.max_context_after = Some(1);
        }
    }

    fn deduplicate_search_results(
        &self,
        results: Vec<SemanticSearchMatch>,
    ) -> (Vec<SemanticSearchMatch>, usize) {
        if let Ok(mut guard) = self.inner.write() {
            let mut seen: HashSet<(String, i32)> = guard
                .recent_hits
                .iter()
                .map(|hit| (hit.path.clone(), hit.chunk_index))
                .collect();

            let mut retained = Vec::with_capacity(results.len());
            let mut duplicates = Vec::new();

            for result in results {
                let key = (result.path.clone(), result.chunk_index);
                if seen.insert(key.clone()) {
                    guard.recent_hits.push(RecentHit {
                        path: key.0,
                        chunk_index: key.1,
                    });
                    retained.push(result);
                } else {
                    duplicates.push(result);
                }
            }

            if guard.recent_hits.len() > RECENT_HIT_HISTORY {
                let excess = guard.recent_hits.len() - RECENT_HIT_HISTORY;
                guard.recent_hits.drain(0..excess);
            }

            if retained.is_empty() && !duplicates.is_empty() {
                if let Some(result) = duplicates.pop() {
                    let key = (result.path.clone(), result.chunk_index);
                    guard.recent_hits.push(RecentHit {
                        path: key.0,
                        chunk_index: key.1,
                    });
                    retained.push(result);
                }
            }

            let removed = duplicates.len();
            (retained, removed)
        } else {
            (results, 0)
        }
    }

    fn build_bundle_meta(&self, usage: &crate::bundle::BundleUsageStats, cache_hit: bool) -> Meta {
        let snapshot = self.snapshot();
        let mut meta = Meta::new();
        meta.insert(
            "bundleUsage".to_string(),
            serde_json::to_value(usage).unwrap_or_else(|_| json!({})),
        );
        meta.insert("cacheHit".to_string(), json!(cache_hit));
        if let Some(remaining) = snapshot.remaining_context_tokens {
            meta.insert("remainingContextTokens".to_string(), json!(remaining));
        }
        meta.insert(
            "effectiveBundleBudget".to_string(),
            json!(snapshot.bundle_budget()),
        );
        meta
    }

    fn build_search_meta(
        &self,
        response: &SemanticSearchResponse,
        duplicates_filtered: usize,
        filters: Option<Value>,
    ) -> Meta {
        let snapshot = self.snapshot();
        let mut meta = Meta::new();
        let mut info = json!({
            "evaluatedChunks": response.evaluated_chunks,
            "resultCount": response.results.len(),
            "summaryMode": response.summary_mode,
            "estimatedTokenCost": estimate_token_cost(&response.results),
            "duplicatesFiltered": duplicates_filtered,
        });
        if let Some(filters) = filters {
            info["filters"] = filters;
        }
        meta.insert("semanticSearch".to_string(), info);
        if let Some(remaining) = snapshot.remaining_context_tokens {
            meta.insert("remainingContextTokens".to_string(), json!(remaining));
        }
        meta
    }

    fn meta_to_value(meta: &Meta) -> Option<Value> {
        let mut map = serde_json::Map::new();
        for (key, value) in meta.iter() {
            map.insert(key.clone(), value.clone());
        }
        if map.is_empty() {
            None
        } else {
            Some(Value::Object(map))
        }
    }

    fn extract_environment_value(value: &Value) -> Option<&Value> {
        value
            .get("environment")
            .or_else(|| value.get("environmentContext"))
            .or_else(|| value.get("environment_context"))
    }

    fn contains_environment_keys(value: &Value) -> bool {
        value.get("cwd").is_some()
            || value.get("bundleBudgetTokens").is_some()
            || value.get("budgetTokens").is_some()
            || value.get("tokenUsage").is_some()
            || value.get("remainingContextTokens").is_some()
    }
}

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
    symbol: Option<SymbolSelector>,
    #[serde(default)]
    ranges: Option<Vec<LineRange>>,
    #[serde(default)]
    focus_line: Option<u32>,
    #[serde(default)]
    max_snippets: Option<u32>,
    #[serde(default)]
    max_neighbors: Option<u32>,
    #[serde(default)]
    budget_tokens: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default)]
    path_contains: Option<String>,
    #[serde(default)]
    classification: Option<Classification>,
    #[serde(default)]
    summary_mode: Option<SummaryMode>,
    #[serde(default)]
    max_context_before: Option<u32>,
    #[serde(default)]
    max_context_after: Option<u32>,
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
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default)]
    path_contains: Option<String>,
    #[serde(default)]
    classification: Option<Classification>,
    #[serde(default)]
    summary_mode: Option<SummaryMode>,
    #[serde(default)]
    max_context_before: Option<u32>,
    #[serde(default)]
    max_context_after: Option<u32>,
}

/// Textual instructions shared with MCP clients.
const SERVER_INSTRUCTIONS_TEMPLATE: &str = r#"Rust rewrite is production-ready. Treat this server as the workspace source of truth and follow this proactive workflow:
1. Prime the index at session start with ingest_codebase {"root": "{ABSOLUTE_ROOT}"} or --watch. Honor .gitignore, skip files larger than 8 MiB, and tune autoEvict/maxDatabaseSizeBytes before the SQLite file balloons. Always pass the absolute workspace root; relative paths often target the wrong codebase.
2. Check index_status before planning or answering. If HEAD moved or isStale is true, ingest again before proceeding.
3. Brief yourself with repository_timeline (and repository_timeline_entry for deep dives) so your plan reflects the latest commits.
4. Use code_lookup in auto mode to assemble payloads: start with query="..." to explore, then request file/symbol bundles for snippets you will cite.
5. Deliver compact payloads—prefer context_bundle with budgetTokens or INDEX_MCP_BUDGET_TOKENS, include citations, and avoid dumping whole files.
6. When you need additional detail, follow up with semantic_search or focused context_bundle calls instead of broad re-ingests.
7. After modifying files, re-run ingest_codebase or rely on watch mode, then confirm freshness with index_status/info so the next task sees the updated payload.

Available tools: ingest_codebase, index_status, code_lookup (search/bundle), semantic_search, context_bundle, repository_timeline, repository_timeline_entry, indexing_guidance, indexing_guidance_tool, info."#;
const INDEXING_GUIDANCE_PROMPT_TEMPLATE: &str = r#"Workflow reminder:
1. Prime the index after a checkout, pull, or edit by running ingest_codebase {"root": "{ABSOLUTE_ROOT}"} (or enabling watch mode); respect .gitignore, skip files >8 MiB, and configure autoEvict/maxDatabaseSizeBytes when needed. Always provide the absolute workspace root to avoid indexing the wrong project.
2. Call index_status before reasoning. If it reports staleness or a HEAD mismatch, ingest before continuing.
3. code_lookup first (query="..." for search, file="..." + symbol for bundles), then semantic_search/context_bundle for refinements.
4. repository_timeline and repository_timeline_entry before planning or applying changes.
5. Keep answers tight: set INDEX_MCP_BUDGET_TOKENS or pass budgetTokens, trim limits, and prefer info/indexing_guidance_tool for diagnostics."#;

fn workspace_root_for_instructions() -> String {
    std::env::current_dir()
        .map(|path| match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) => path,
        })
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "/ABSOLUTE/PATH/TO/WORKSPACE".to_string())
}

fn render_instruction(template: &str) -> String {
    template.replace("{ABSOLUTE_ROOT}", &workspace_root_for_instructions())
}

fn server_instructions() -> String {
    render_instruction(SERVER_INSTRUCTIONS_TEMPLATE)
}

fn indexing_guidance_prompt_text() -> String {
    render_instruction(INDEXING_GUIDANCE_PROMPT_TEMPLATE)
}

/// Primary server state for the Rust MCP implementation.
#[derive(Clone)]
pub struct IndexMcpService {
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    environment: EnvironmentState,
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

        tokio::spawn(async {
            match tokio::task::spawn_blocking(|| warm_up_embedder(None)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(?error, "Embedder warm-up failed"),
                Err(join_error) => warn!(?join_error, "Embedder warm-up task cancelled"),
            }
        });

        Ok(Self {
            tool_router,
            prompt_router,
            environment: EnvironmentState::new(),
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
                indexing_guidance_prompt_text(),
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
        Parameters(mut params): Parameters<IngestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.environment.update_from_meta(&ctx.meta);
        self.environment.apply_ingest_defaults(&mut params);

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
        Parameters(mut params): Parameters<SemanticSearchRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.environment.update_from_meta(&ctx.meta);
        self.environment.apply_semantic_defaults(&mut params);
        let filter_summary = build_search_filter_summary(&params);
        let search_params = SemanticSearchParams {
            root: params.root.clone(),
            query: params.query.clone(),
            database_name: params.database_name.clone(),
            limit: params.limit,
            model: params.model.clone(),
            language: params.language.clone(),
            path_prefix: params.path_prefix.clone(),
            path_contains: params.path_contains.clone(),
            classification: params.classification.clone(),
            summary_mode: params.summary_mode,
            max_context_before: params.max_context_before,
            max_context_after: params.max_context_after,
        };

        let mut response = semantic_search(search_params)
            .await
            .map_err(convert_semantic_search_error)?;

        let (deduplicated, duplicates_filtered) = self
            .environment
            .deduplicate_search_results(response.results);
        response.results = deduplicated;

        let snapshot = self.environment.snapshot();
        response.suggested_tools = build_search_suggestions(&snapshot, &response);

        let meta =
            self.environment
                .build_search_meta(&response, duplicates_filtered, filter_summary);
        build_semantic_search_result(response, meta)
    }

    #[tool(
        name = "context_bundle",
        description = "Return file-level definitions, snippets, and related graph neighbors."
    )]
    async fn context_bundle_tool(
        &self,
        Parameters(mut params): Parameters<ContextBundleParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.environment.update_from_meta(&ctx.meta);
        self.environment.apply_bundle_defaults(&mut params);
        let response = context_bundle(params)
            .await
            .map_err(convert_context_bundle_error)?;

        let meta = self
            .environment
            .build_bundle_meta(&response.usage, response.usage.cache_hit);

        build_context_bundle_result(response, Some(meta))
    }

    #[tool(
        name = "code_lookup",
        description = "Route lookups to semantic search (search mode only)."
    )]
    async fn code_lookup(
        &self,
        Parameters(mut params): Parameters<CodeLookupParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.environment.update_from_meta(&ctx.meta);
        self.environment.apply_code_lookup_defaults(&mut params);
        let CodeLookupParams {
            root,
            database_name,
            mode,
            query,
            file,
            symbol,
            ranges,
            focus_line,
            max_snippets,
            max_neighbors,
            budget_tokens,
            limit,
            model,
            language,
            path_prefix,
            path_contains,
            classification,
            summary_mode,
            max_context_before,
            max_context_after,
        } = params;

        let resolved_mode = mode.unwrap_or_else(|| {
            if query.as_ref().is_some_and(|value| !value.trim().is_empty()) {
                "search".to_string()
            } else if file.as_ref().is_some_and(|value| !value.trim().is_empty()) {
                "bundle".to_string()
            } else {
                "search".to_string()
            }
        });

        match resolved_mode.as_str() {
            "search" => {
                let query = query.ok_or_else(|| {
                    McpError::invalid_params("code_lookup search mode requires a query.", None)
                })?;

                let search_params = SemanticSearchParams {
                    root,
                    query,
                    database_name,
                    limit,
                    model,
                    language: language.clone(),
                    path_prefix: path_prefix.clone(),
                    path_contains: path_contains.clone(),
                    classification: classification.clone(),
                    summary_mode,
                    max_context_before,
                    max_context_after,
                };

                let mut response = semantic_search(search_params)
                    .await
                    .map_err(convert_semantic_search_error)?;
                let (deduplicated, duplicates_filtered) = self
                    .environment
                    .deduplicate_search_results(response.results);
                response.results = deduplicated;
                let snapshot = self.environment.snapshot();
                response.suggested_tools = build_search_suggestions(&snapshot, &response);
                let filter_summary = build_lookup_filter_summary(
                    &language,
                    &path_prefix,
                    &path_contains,
                    &classification,
                );
                let meta = self.environment.build_search_meta(
                    &response,
                    duplicates_filtered,
                    filter_summary,
                );
                build_code_lookup_result(resolved_mode, response, Some(meta))
            }
            "bundle" => {
                let file = file.or(query).ok_or_else(|| {
                    McpError::invalid_params("code_lookup bundle mode requires a file path.", None)
                })?;

                let mut bundle_params = ContextBundleParams {
                    root,
                    database_name,
                    file,
                    symbol,
                    max_snippets: max_snippets.or(limit),
                    max_neighbors,
                    budget_tokens,
                    ranges,
                    focus_line,
                };
                self.environment.apply_bundle_defaults(&mut bundle_params);

                let response = context_bundle(bundle_params)
                    .await
                    .map_err(convert_context_bundle_error)?;
                let meta = self
                    .environment
                    .build_bundle_meta(&response.usage, response.usage.cache_hit);

                build_code_lookup_bundle_response(resolved_mode, response, Some(meta))
            }
            _ => Err(McpError::invalid_params(
                "Unsupported code_lookup mode. Supported modes: search, bundle.",
                None,
            )),
        }
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
        name = "repository_timeline_entry",
        description = "Fetch a stored repository timeline entry, including full diff text if available."
    )]
    async fn repository_timeline_entry_tool(
        &self,
        Parameters(params): Parameters<RepositoryTimelineEntryLookupParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let response = repository_timeline_entry_detail(params)
            .await
            .map_err(convert_repository_timeline_error)?;

        build_repository_timeline_entry_result(response)
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
            instructions: Some(server_instructions()),
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

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn summarize_ingest(payload: &IngestResponse) -> String {
    let mut summary = format!(
        "Indexed {} file(s) ({} chunk(s)) at {} in {:.2}s.",
        payload.ingested_file_count,
        payload.embedded_chunk_count,
        payload.root,
        payload.duration_ms as f64 / 1000.0
    );

    summary.push_str(&format!(
        " Database size is {}.",
        format_bytes(payload.database_size_bytes)
    ));

    if let Some(model) = &payload.embedding_model {
        summary.push_str(&format!(" Embedding model {}.", model));
    }

    if let Some(reused) = payload.reused_file_count {
        summary.push_str(&format!(
            " Reused cached embeddings for {} unchanged file(s).",
            reused
        ));
    }

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

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
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

    if let Some(size) = payload.database_size_bytes {
        summary.push_str(&format!(" Size {}.", format_bytes(size)));
    }

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
        let indexed = payload
            .commit_sha
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "unknown".to_string());
        let current = payload
            .current_commit_sha
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "unknown".to_string());
        summary.push_str(&format!(
            " Index is stale (stored {} vs. workspace {}).",
            indexed, current
        ));
    } else if let Some(commit) = payload.commit_sha.as_deref() {
        summary.push_str(&format!(
            " Index aligned with commit {}.",
            short_sha(commit)
        ));
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
    meta: Meta,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_semantic_search(&response);
    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize semantic search result: {error}"),
            None,
        )
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta: Some(meta),
    })
}

fn build_search_suggestions(
    snapshot: &EnvironmentSnapshot,
    response: &SemanticSearchResponse,
) -> Vec<SuggestedTool> {
    const MAX_SUGGESTIONS: usize = 3;
    if response.results.is_empty() {
        return Vec::new();
    }

    let budget_hint = snapshot.bundle_budget().min(u32::MAX as usize) as u32;

    response
        .results
        .iter()
        .take(MAX_SUGGESTIONS)
        .enumerate()
        .map(|(index, result)| {
            let mut params = Map::new();
            if let Some(cwd) = snapshot.cwd.clone() {
                params.insert("root".to_string(), json!(cwd));
            }
            params.insert("file".to_string(), json!(result.path));
            if let Some(database_name) = response.database_name.as_ref() {
                params.insert("databaseName".to_string(), json!(database_name));
            }
            params.insert("maxSnippets".to_string(), json!(DEFAULT_SNIPPET_LIMIT_HINT));
            params.insert("maxNeighbors".to_string(), json!(6));
            params.insert("budgetTokens".to_string(), json!(budget_hint));

            let mut description = result.path.clone();

            if let Some(start_line_raw) = result
                .line_start
                .and_then(|line| (line > 0).then_some(line as u32))
            {
                let end_line_raw = result
                    .line_end
                    .and_then(|line| (line > 0).then_some(line as u32))
                    .unwrap_or(start_line_raw);
                let (min_line, max_line) = if end_line_raw < start_line_raw {
                    (end_line_raw, start_line_raw)
                } else {
                    (start_line_raw, end_line_raw)
                };

                let padded_start = min_line.saturating_sub(SUGGESTED_RANGE_PADDING).max(1);
                let padded_end = max_line.saturating_add(SUGGESTED_RANGE_PADDING);
                let focus_line = min_line + (max_line.saturating_sub(min_line)) / 2;

                params.insert("focusLine".to_string(), json!(focus_line));
                params.insert(
                    "ranges".to_string(),
                    json!([{"startLine": padded_start, "endLine": padded_end}]),
                );

                if padded_start == padded_end {
                    description = format!("{}#L{}", result.path, padded_start);
                } else {
                    description = format!("{}:{}-{}", result.path, padded_start, padded_end);
                }
            }

            let preview = snippet_preview(&result.content, result.context_before.as_deref());

            SuggestedTool {
                tool: "context_bundle".to_string(),
                rank: (index as u32) + 1,
                score: result.normalized_score,
                description: Some(description),
                preview,
                parameters: Value::Object(params),
            }
        })
        .collect()
}

fn snippet_preview(content: &str, context_before: Option<&str>) -> Option<String> {
    let mut fragments = Vec::new();
    if let Some(before) = context_before {
        let trimmed = before.trim();
        if !trimmed.is_empty() {
            fragments.push(trimmed);
        }
    }

    let trimmed = content.trim();
    if trimmed.is_empty() {
        if fragments.is_empty() {
            return None;
        }
        return Some(compose_preview(&fragments));
    }
    fragments.push(trimmed);

    Some(compose_preview(&fragments))
}

fn compose_preview(segments: &[&str]) -> String {
    const MAX_PREVIEW_LEN: usize = 160;
    let joined = segments
        .iter()
        .flat_map(|segment| segment.lines())
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");

    let trimmed = joined.trim();
    let mut chars = trimmed.chars();
    let mut preview: String = chars.by_ref().take(MAX_PREVIEW_LEN).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn build_code_lookup_result(
    mode: String,
    search_result: SemanticSearchResponse,
    meta: Option<Meta>,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_semantic_search(&search_result);
    let payload = CodeLookupResponse {
        mode,
        summary: summary.clone(),
        search_result: Some(search_result),
        bundle_result: None,
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize code_lookup result: {error}"),
            None,
        )
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta,
    })
}

fn build_code_lookup_bundle_response(
    mode: String,
    bundle: ContextBundleResponse,
    meta: Option<Meta>,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_bundle(&bundle);

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
    };

    let value: Value = serde_json::to_value(&payload).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize code_lookup result: {error}"),
            None,
        )
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta,
    })
}

fn summarize_bundle(bundle: &ContextBundleResponse) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "Context bundle prepared for {} with {} definition(s) and {} snippet(s).",
        bundle.file.path,
        bundle.definitions.len(),
        bundle.snippets.len()
    ));

    if let Some(focus) = &bundle.focus_definition {
        parts.push(format!("Focus on {} {}.", focus.kind, focus.name));
    } else if let Some(primary) = bundle.definitions.first() {
        parts.push(format!(
            "Primary definition {} {}.",
            primary.kind, primary.name
        ));
    }

    match summarize_snippets(bundle) {
        Some(detail) => parts.push(detail),
        None => {
            parts.push("Snippets: none captured; adjust selection or increase limits.".to_string())
        }
    }

    if let Some(link) = bundle.quick_links.first() {
        let label = match link.r#type {
            QuickLinkType::File => format!("file {}", link.label),
            QuickLinkType::RelatedSymbol => format!("symbol {}", link.label),
        };
        parts.push(format!("First quick link: {}.", label));
    }

    parts.push(format!(
        "Token usage {} of {} ({} unused).",
        bundle.usage.used_tokens, bundle.usage.budget_tokens, bundle.usage.remaining_tokens
    ));
    if bundle.usage.cache_hit {
        parts.push("Served from cache.".to_string());
    }

    if !bundle.warnings.is_empty() {
        let warning_excerpt = bundle
            .warnings
            .first()
            .map(|first| first.as_str())
            .unwrap_or_default();
        let warning_note = if bundle.warnings.len() > 1 {
            format!(
                "Warnings: {} (first: {}).",
                bundle.warnings.len(),
                warning_excerpt
            )
        } else {
            format!("Warning: {}.", warning_excerpt)
        };
        parts.push(warning_note);
    }

    parts.join(" ")
}

fn summarize_snippets(bundle: &ContextBundleResponse) -> Option<String> {
    if bundle.snippets.is_empty() {
        return None;
    }

    let token_estimate: usize = bundle
        .snippets
        .iter()
        .map(|snippet| approx_token_count(&snippet.content))
        .sum();

    let mut descriptors: Vec<String> = bundle
        .snippets
        .iter()
        .take(3)
        .map(|snippet| {
            let source = match snippet.source {
                SnippetSource::Chunk => "chunk",
                SnippetSource::Content => "content",
            };
            let span = match (snippet.line_start, snippet.line_end) {
                (Some(start), Some(end)) if start == end => format!("line {start}"),
                (Some(start), Some(end)) => format!("lines {start}-{end}"),
                (Some(start), None) => format!("line {start}"),
                _ => "lines n/a".to_string(),
            };
            format!("{source} {span}")
        })
        .collect();

    if bundle.snippets.len() > descriptors.len() {
        descriptors.push(format!(
            "+{} more",
            bundle.snippets.len() - descriptors.len()
        ));
    }

    Some(format!(
        "Snippets: {} (~{} token(s)).",
        descriptors.join(", "),
        token_estimate
    ))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn approx_token_count(text: &str) -> usize {
    ((text.len() as f64 / 4.0).ceil()) as usize
}

fn estimate_token_cost(results: &[SemanticSearchMatch]) -> usize {
    let total_chars: usize = results
        .iter()
        .map(|result| {
            result.content.len()
                + result
                    .context_before
                    .as_ref()
                    .map(|value| value.len())
                    .unwrap_or(0)
                + result
                    .context_after
                    .as_ref()
                    .map(|value| value.len())
                    .unwrap_or(0)
        })
        .sum();

    ((total_chars as f64) / 4.0).ceil() as usize
}

fn build_search_filter_summary(request: &SemanticSearchRequest) -> Option<Value> {
    filters_to_value(
        &request.language,
        &request.path_prefix,
        &request.path_contains,
        &request.classification,
    )
}

fn build_lookup_filter_summary(
    language: &Option<String>,
    path_prefix: &Option<String>,
    path_contains: &Option<String>,
    classification: &Option<Classification>,
) -> Option<Value> {
    filters_to_value(language, path_prefix, path_contains, classification)
}

fn filters_to_value(
    language: &Option<String>,
    path_prefix: &Option<String>,
    path_contains: &Option<String>,
    classification: &Option<Classification>,
) -> Option<Value> {
    let mut map = Map::new();
    if let Some(language) = language.as_ref() {
        if !language.trim().is_empty() {
            map.insert("language".to_string(), json!(language));
        }
    }
    if let Some(prefix) = path_prefix.as_ref() {
        if !prefix.trim().is_empty() {
            map.insert("pathPrefix".to_string(), json!(prefix));
        }
    }
    if let Some(fragment) = path_contains.as_ref() {
        if !fragment.trim().is_empty() {
            map.insert("pathContains".to_string(), json!(fragment));
        }
    }
    if let Some(classification) = classification.as_ref() {
        map.insert("classification".to_string(), json!(classification));
    }

    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
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
            if source.kind() == ErrorKind::NotFound {
                McpError::invalid_params(
                    format!(
                        "File '{path}' is not cached; run ingest_codebase to refresh the index."
                    ),
                    None,
                )
            } else {
                McpError::internal_error(format!("Failed to access '{path}': {source}"), None)
            }
        }
        ContextBundleError::Join(source) => {
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
        RepositoryTimelineError::Database { path, source } => {
            McpError::internal_error(format!("SQLite error at {path}: {source}"), None)
        }
        RepositoryTimelineError::Serialization(source) => McpError::internal_error(
            format!("Failed to serialize repository timeline data: {source}"),
            None,
        ),
        RepositoryTimelineError::EntryNotFound { commit_sha, path } => McpError::invalid_params(
            format!("Commit {commit_sha} not found in timeline cache at {path}"),
            None,
        ),
    }
}

fn build_context_bundle_result(
    response: ContextBundleResponse,
    meta: Option<Meta>,
) -> Result<CallToolResult, McpError> {
    let summary = summarize_bundle(&response);

    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize context bundle result: {error}"),
            None,
        )
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta,
    })
}

fn build_repository_timeline_result(
    response: RepositoryTimelineResponse,
) -> Result<CallToolResult, McpError> {
    let mut summary = if response.total_commits == 0 {
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

    if response.include_diffs {
        summary
            .push_str(" Diffs cached in SQLite; call repository_timeline_entry for full output.");
    }

    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize repository timeline result: {error}"),
            None,
        )
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_repository_timeline_entry_result(
    response: RepositoryTimelineEntryLookupResponse,
) -> Result<CallToolResult, McpError> {
    let diff_len = response.diff.as_ref().map(|diff| diff.len()).unwrap_or(0);
    let summary = if diff_len > 0 {
        format!(
            "repository_timeline_entry: retrieved diff for commit {} ({} bytes cached).",
            response.entry.sha, diff_len
        )
    } else {
        format!(
            "repository_timeline_entry: no diff stored for commit {}.",
            response.entry.sha
        )
    };

    let value: Value = serde_json::to_value(&response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to serialize repository timeline entry result: {error}"),
            None,
        )
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{
        BundleDefinition, BundleFileMetadata, BundleSnippet, ContextBundleQuickLink,
        ContextBundleResponse, QuickLinkType, SnippetSource,
    };
    use crate::index_status::{IndexStatusIngestion, IndexStatusResponse};
    use crate::ingest::IngestResponse;
    use crate::search::{Classification, SemanticSearchMatch, SemanticSearchResponse};
    use serde_json::json;

    #[test]
    fn summarize_ingest_reports_key_metrics() {
        let payload = IngestResponse {
            root: "/workspace".into(),
            database_path: "/workspace/.mcp-index.sqlite".into(),
            database_size_bytes: 1_024,
            ingested_file_count: 3,
            skipped: Vec::new(),
            deleted_paths: Vec::new(),
            duration_ms: 1_500,
            embedded_chunk_count: 42,
            embedding_model: Some("Xenova/all-MiniLM-L6-v2".into()),
            graph_node_count: 0,
            graph_edge_count: 0,
            evicted: None,
            reused_file_count: Some(1),
        };

        let summary = summarize_ingest(&payload);

        assert!(summary.contains("(42 chunk(s))"));
        assert!(summary.contains("Database size is 1.0 KiB."));
        assert!(summary.contains("Embedding model Xenova/all-MiniLM-L6-v2."));
    }

    #[test]
    fn summarize_index_status_highlights_stale_commit_delta() {
        let latest = IndexStatusIngestion {
            id: "ingest-1".into(),
            root: "/workspace".into(),
            started_at: 0,
            finished_at: 1,
            duration_ms: 750,
            file_count: 12,
            skipped_count: 0,
            deleted_count: 0,
        };

        let payload = IndexStatusResponse {
            database_path: "/workspace/.mcp-index.sqlite".into(),
            database_exists: true,
            database_size_bytes: Some(10_485_760),
            total_files: 64,
            total_chunks: 512,
            embedding_models: vec!["model-A".into(), "model-B".into()],
            total_graph_nodes: 0,
            total_graph_edges: 0,
            latest_ingestion: Some(latest.clone()),
            recent_ingestions: vec![latest],
            commit_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            indexed_at: Some(0),
            current_commit_sha: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
            is_stale: true,
        };

        let summary = summarize_index_status(&payload);

        assert!(summary.contains("Size 10.0 MiB."));
        assert!(summary.contains("Index is stale (stored aaaaaaa vs. workspace bbbbbbb)."));
        assert!(summary.contains("Embedding models: model-A, model-B."));
    }

    #[test]
    fn summarize_bundle_surfaces_primary_snippets_and_links() {
        let bundle = ContextBundleResponse {
            database_path: "db.sqlite".into(),
            file: BundleFileMetadata {
                path: "src/lib.rs".into(),
                size: 128,
                modified: 1_710_000_000,
                hash: "abc123".into(),
                last_indexed_at: 1_710_000_123,
                brief: None,
                content: None,
            },
            definitions: vec![BundleDefinition {
                id: "def-1".into(),
                name: "foo".into(),
                kind: "function".into(),
                signature: Some("fn foo()".into()),
                range_start: Some(1),
                range_end: Some(10),
                metadata: None,
                visibility: Some("pub".into()),
                docstring: None,
                todo_count: None,
            }],
            focus_definition: None,
            related: Vec::new(),
            snippets: vec![BundleSnippet {
                source: SnippetSource::Chunk,
                chunk_index: Some(0),
                content: "fn foo() {}".into(),
                byte_start: Some(0),
                byte_end: Some(12),
                line_start: Some(1),
                line_end: Some(1),
                served_count: None,
            }],
            latest_ingestion: None,
            warnings: vec!["No graph metadata".into()],
            quick_links: vec![ContextBundleQuickLink {
                r#type: QuickLinkType::File,
                label: "src/lib.rs".into(),
                path: Some("src/lib.rs".into()),
                direction: None,
                symbol_id: None,
                symbol_kind: None,
            }],
            usage: crate::bundle::BundleUsageStats {
                definitions_tokens: 10,
                snippet_tokens: 12,
                used_tokens: 22,
                budget_tokens: 3_000,
                remaining_tokens: 2_978,
                omitted_snippets: 0,
                excerpt_snippets: 0,
                summary_snippets: 0,
                cache_hit: false,
            },
        };

        let summary = summarize_bundle(&bundle);

        assert!(summary.contains(
            "Context bundle prepared for src/lib.rs with 1 definition(s) and 1 snippet(s)."
        ));
        assert!(summary.contains("Primary definition function foo."));
        assert!(summary.contains("Snippets: chunk line 1 (~3 token(s))."));
        assert!(summary.contains("First quick link: file src/lib.rs."));
        assert!(summary.contains("Warning: No graph metadata."));
    }

    #[test]
    fn summarize_semantic_search_reports_top_hit_and_score() {
        let response = SemanticSearchResponse {
            database_path: "db.sqlite".into(),
            database_name: Some("db.sqlite".into()),
            embedding_model: Some("custom-model".into()),
            total_chunks: 1_000,
            evaluated_chunks: 250,
            results: vec![SemanticSearchMatch {
                path: "src/main.rs".into(),
                chunk_index: 0,
                score: 0.92,
                normalized_score: 0.87,
                language: Some("Rust".into()),
                classification: Classification::Function,
                content: "fn main() {}".into(),
                embedding_model: "custom-model".into(),
                byte_start: None,
                byte_end: None,
                line_start: Some(42),
                line_end: Some(45),
                context_before: None,
                context_after: None,
            }],
            summary_mode: SummaryMode::Brief,
            suggested_tools: Vec::new(),
        };

        let summary = crate::search::summarize_semantic_search(&response);

        assert!(summary.contains(
            "Semantic search scanned 250 chunk(s) and returned 1 match(es) (model custom-model)."
        ));
        assert!(summary.contains("Top hit: src/main.rs#L42 (score 0.87)."));
    }

    #[test]
    fn build_search_suggestions_embed_ranges_and_focus_line() {
        let snapshot = EnvironmentSnapshot {
            cwd: Some("/workspace".into()),
            bundle_budget_override: Some(1_600),
            remaining_context_tokens: Some(3_200),
            recent_hits: Vec::new(),
        };

        let response = SemanticSearchResponse {
            database_path: "db.sqlite".into(),
            database_name: Some("db.sqlite".into()),
            embedding_model: Some("model".into()),
            total_chunks: 100,
            evaluated_chunks: 50,
            results: vec![SemanticSearchMatch {
                path: "src/lib.rs".into(),
                chunk_index: 7,
                score: 0.91,
                normalized_score: 0.82,
                language: Some("Rust".into()),
                classification: Classification::Function,
                content: "fn sample() { /* ... */ }".into(),
                embedding_model: "model".into(),
                byte_start: None,
                byte_end: None,
                line_start: Some(40),
                line_end: Some(44),
                context_before: None,
                context_after: None,
            }],
            summary_mode: SummaryMode::Brief,
            suggested_tools: Vec::new(),
        };

        let suggestions = build_search_suggestions(&snapshot, &response);
        assert_eq!(suggestions.len(), 1);
        let suggestion = &suggestions[0];
        assert_eq!(suggestion.tool, "context_bundle");
        let params = suggestion
            .parameters
            .as_object()
            .expect("parameters object");
        assert_eq!(params.get("focusLine"), Some(&json!(42)));

        let ranges = params
            .get("ranges")
            .and_then(|value| value.as_array())
            .expect("ranges array");
        assert_eq!(ranges.len(), 1);
        let range = ranges[0].as_object().expect("range object");
        assert_eq!(range.get("startLine"), Some(&json!(38)));
        assert_eq!(range.get("endLine"), Some(&json!(46)));
    }

    #[test]
    fn code_lookup_infers_bundle_mode_from_file_path_when_mode_missing() {
        let env = EnvironmentState::new();
        let mut params = CodeLookupParams {
            root: None,
            database_name: None,
            mode: None,
            query: None,
            file: Some("src/lib.rs".into()),
            symbol: None,
            ranges: None,
            focus_line: None,
            max_snippets: None,
            max_neighbors: None,
            budget_tokens: None,
            limit: None,
            model: None,
            language: None,
            path_prefix: None,
            path_contains: None,
            classification: None,
            summary_mode: None,
            max_context_before: None,
            max_context_after: None,
        };

        env.apply_code_lookup_defaults(&mut params);

        let resolved_mode = params.mode.clone().unwrap_or_else(|| {
            if params
                .query
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                "search".to_string()
            } else if params
                .file
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                "bundle".to_string()
            } else {
                "search".to_string()
            }
        });

        assert_eq!(resolved_mode, "bundle");
    }
}
