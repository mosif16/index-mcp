use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::bundle::{
    context_bundle, BundleDefinition, BundleDiagnostics, BundleEdgeNeighbor, BundleFileMetadata,
    BundleIngestionSummary, BundleSnippet, BundleUsageStats, ContextBundleParams,
    ContextBundleQuickLink, ContextBundleResponse, LineRange, NeighborDirection, NeighborNode,
    QuickLinkType, SnippetSource, SymbolSelector,
};
use crate::git_timeline::{
    repository_timeline, repository_timeline_entry_detail, RepositoryTimelineDiffSummary,
    RepositoryTimelineDirectoryChurn, RepositoryTimelineEntry, RepositoryTimelineEntryLookupParams,
    RepositoryTimelineEntryLookupResponse, RepositoryTimelineError, RepositoryTimelineFileChange,
    RepositoryTimelineParams, RepositoryTimelineResponse, RepositoryTimelineTopFile,
    TimelineIdentity,
};
use crate::index_status::{
    get_index_status, IndexStatusError, IndexStatusIngestion, IndexStatusParams,
    IndexStatusResponse,
};
use crate::ingest::{
    ingest_codebase, warm_up_embedder, EvictionReport, IngestError, IngestParams, IngestResponse,
    SkippedFile,
};
use crate::remote_proxy::RemoteProxyRegistry;
use crate::search::{
    semantic_search, summarize_semantic_search, Classification, ContextGoal, QueryIntent,
    SearchDiagnostics, SearchResultCoordinate, SearchSource, SemanticSearchError,
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
        if !response.clarification_prompts.is_empty() {
            info["clarifications"] = json!(response.clarification_prompts);
        }
        if !response.context_goals.is_empty() {
            info["contextGoals"] = json!(response.context_goals);
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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
    #[allow(dead_code)]
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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct IngestWithStatusParams {
    #[serde(flatten)]
    ingest: IngestParams,
    #[serde(default)]
    history_limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
struct SearchInclude {
    #[serde(default = "default_true")]
    bundle: bool,
    #[serde(default = "default_true")]
    lookup: bool,
}

impl Default for SearchInclude {
    fn default() -> Self {
        Self {
            bundle: true,
            lookup: true,
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct SharedBudgetSpec {
    #[serde(default)]
    total_tokens: Option<u32>,
    #[serde(default)]
    search_tokens: Option<u32>,
    #[serde(default)]
    bundle_tokens: Option<u32>,
    #[serde(default)]
    lookup_tokens: Option<u32>,
    #[serde(default)]
    deadline_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
struct UnifiedSemanticSearchRequest {
    #[serde(flatten)]
    search: SemanticSearchRequest,
    #[serde(default)]
    include: SearchInclude,
    #[serde(default)]
    bundle: Option<ContextBundleParams>,
    #[serde(default)]
    lookup: Option<CodeLookupParams>,
    #[serde(default)]
    shared_budget: Option<SharedBudgetSpec>,
}

#[derive(Debug, Clone, Default)]
struct SharedBudget {
    total_tokens: Option<u32>,
    bundle_tokens: Option<u32>,
    lookup_tokens: Option<u32>,
}

impl SharedBudget {
    fn from_spec(spec: Option<SharedBudgetSpec>) -> Self {
        if let Some(spec) = spec {
            Self {
                total_tokens: spec.total_tokens,
                bundle_tokens: spec.bundle_tokens,
                lookup_tokens: spec.lookup_tokens,
            }
        } else {
            Self::default()
        }
    }

    fn bundle_budget_hint(&self, snapshot: &EnvironmentSnapshot) -> Option<u32> {
        let mut budget = snapshot.bundle_budget().min(u32::MAX as usize) as u32;
        if let Some(limit) = self.total_tokens {
            budget = budget.min(limit);
        }
        if let Some(limit) = self.bundle_tokens {
            budget = budget.min(limit);
        }
        Some(budget.max(MIN_BUNDLE_BUDGET as u32))
    }
}

#[derive(Debug, Clone)]
struct OrchestrationPlan {
    search: SemanticSearchRequest,
    include_bundle: bool,
    include_lookup: bool,
    bundle_override: Option<ContextBundleParams>,
    lookup_override: Option<CodeLookupParams>,
    shared_budget: SharedBudget,
}

impl OrchestrationPlan {
    fn should_run_bundle(&self) -> bool {
        self.include_bundle || self.bundle_override.is_some()
    }

    fn should_run_lookup(&self) -> bool {
        self.include_lookup || self.lookup_override.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentOutcome {
    NotRequested,
    Success,
    Skipped,
    Failed,
}

impl Default for AttachmentOutcome {
    fn default() -> Self {
        Self::NotRequested
    }
}

impl AttachmentOutcome {
    fn as_label(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::Success => "success",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }

    fn is_failure(self) -> bool {
        matches!(self, Self::Failed)
    }
}

impl From<&AttachmentResult> for AttachmentOutcome {
    fn from(result: &AttachmentResult) -> Self {
        match result {
            AttachmentResult::Success { .. } => Self::Success,
            AttachmentResult::Skipped { .. } => Self::Skipped,
            AttachmentResult::Failed { .. } => Self::Failed,
        }
    }
}

#[derive(Default)]
struct SearchAttachmentAccumulator {
    bundle_value: Option<Value>,
    bundle_meta: Option<Meta>,
    bundle_summary: Option<String>,
    lookup_value: Option<Value>,
    lookup_meta: Option<Meta>,
    lookup_summary: Option<String>,
    warnings: Vec<String>,
    bundle_outcome: AttachmentOutcome,
    lookup_outcome: AttachmentOutcome,
}

impl SearchAttachmentAccumulator {
    fn push_warning(&mut self, message: String) {
        self.warnings.push(message);
    }

    fn bundle(&mut self, result: AttachmentResult) {
        self.bundle_outcome = AttachmentOutcome::from(&result);
        match result {
            AttachmentResult::Success {
                structured,
                meta,
                summary,
            } => {
                self.bundle_value = Some(structured);
                self.bundle_meta = meta;
                self.bundle_summary = Some(summary);
            }
            AttachmentResult::Skipped { warning } => self.push_warning(warning),
            AttachmentResult::Failed { warning } => self.push_warning(warning),
        }
    }

    fn lookup(&mut self, result: AttachmentResult) {
        self.lookup_outcome = AttachmentOutcome::from(&result);
        match result {
            AttachmentResult::Success {
                structured,
                meta,
                summary,
            } => {
                self.lookup_value = Some(structured);
                self.lookup_meta = meta;
                self.lookup_summary = Some(summary);
            }
            AttachmentResult::Skipped { warning } => self.push_warning(warning),
            AttachmentResult::Failed { warning } => self.push_warning(warning),
        }
    }

    fn attachments_value(&self) -> Option<Value> {
        if self.bundle_value.is_none() && self.lookup_value.is_none() {
            return None;
        }
        let mut map = serde_json::Map::new();
        if let Some(bundle) = self.bundle_value.clone() {
            map.insert("bundle".to_string(), bundle);
        }
        if let Some(code) = self.lookup_value.clone() {
            map.insert("code".to_string(), code);
        }
        Some(Value::Object(map))
    }

    fn attachments_meta(&self) -> Option<Value> {
        if self.bundle_meta.is_none() && self.lookup_meta.is_none() {
            return None;
        }
        let mut map = serde_json::Map::new();
        if let Some(meta) = self.bundle_meta.clone() {
            if let Ok(value) = serde_json::to_value(meta) {
                if !value.is_null() {
                    map.insert("bundle".to_string(), value);
                }
            }
        }
        if let Some(meta) = self.lookup_meta.clone() {
            if let Ok(value) = serde_json::to_value(meta) {
                if !value.is_null() {
                    map.insert("code".to_string(), value);
                }
            }
        }
        if map.is_empty() {
            None
        } else {
            Some(Value::Object(map))
        }
    }
}

enum AttachmentResult {
    Success {
        structured: Value,
        meta: Option<Meta>,
        summary: String,
    },
    Skipped {
        warning: String,
    },
    Failed {
        warning: String,
    },
}

/// Textual instructions shared with MCP clients.
const SERVER_INSTRUCTIONS_TEMPLATE: &str = r#"Rust rewrite is production-ready. Treat this server as the workspace source of truth and follow this proactive workflow:
1. Prime the index at session start with index_refresh {"root": "{ABSOLUTE_ROOT}"} (combines ingest_codebase + index_status in one response) or --watch. Honor .gitignore, skip files larger than 8 MiB, and tune autoEvict/maxDatabaseSizeBytes before the SQLite file balloons. Always pass the absolute workspace root; relative paths often target the wrong codebase. If you override databaseName, supply a filename (for example ".mcp-index.sqlite"); directory-style values like "." cause SQLite to reject the request.
2. If you skip index_refresh (or only need a quick status check), call index_status before planning or answering. If HEAD moved or isStale is true, ingest again before proceeding.
3. Brief yourself with repository_timeline (and repository_timeline_entry for deep dives) so your plan reflects the latest commits.
4. Locate targets with semantic_search (query mode) and let the unified attachments decorate results. Set include.bundle/include.lookup or provide override payloads when you need heavier bundles, and keep summaryMode (brief/compressed), focusDefinition, and maxSnippets/maxNeighbors tuned so responses stay focused. The server tracks recently delivered chunks—pass recent_hits to suppress repeats or reset it when you need fresh spans.
5. Shape attachments to your window: supply budgetTokens (or INDEX_MCP_BUDGET_TOKENS), trim snippet limits, and disable include.bundle/include.lookup for lean responses. semantic_search already highlights the focus span, so avoid whole-file dumps unless explicitly required.
6. Add detail iteratively: issue additional semantic_search passes with adjusted filters instead of broad re-ingests. If dedupe hides something important, request a different chunk index or clear recent_hits rather than re-requesting the entire query.
7. After modifying files, re-run index_refresh (or ingest_codebase followed by index_status) or rely on watch mode so the next task sees the updated payload.
Responses now return compact structured_content: summaries stay in the text content, while JSON payloads use short keys (for example t:"sem"/"ctx", defs/sn for bundles, r for results). Prefer the structured data for programmatic handling and avoid relying on legacy CamelCase fields. Unified semantic_search calls default to returning bundle and code attachments under structured_content.att with any issues surfaced in warn; set include.bundle/include.lookup to false when you need slimmer responses.
Info telemetry now lives under meta.semanticSearch: check clarifications/contextGoals, filter summaries, duplicate counts, and token estimates before re-querying. structured_content.clar repeats the prompts for easy display—surface them and adjust the next query. meta.attachments captures bundle/lookup budgets, cache hits, and remainingContextTokens so you can plan follow-up calls without re-inspecting the raw payload.

Available tools: ingest_codebase, index_refresh, semantic_search, index_status, repository_timeline, repository_timeline_entry, info."#;
const INDEXING_GUIDANCE_PROMPT_TEMPLATE: &str = r#"Workflow reminder:
1. Prime the index after a checkout, pull, or edit by running index_refresh {"root": "{ABSOLUTE_ROOT}"} (combines ingest_codebase + index_status in one response) or enabling watch mode; respect .gitignore, skip files >8 MiB, and configure autoEvict/maxDatabaseSizeBytes when needed. Always provide the absolute workspace root to avoid indexing the wrong project. If you override databaseName, make it a filename (for example ".mcp-index.sqlite"); directory-like values will fail to open.
2. If you skip index_refresh (or only need a quick status check), call index_status before reasoning. If it reports staleness or a HEAD mismatch, ingest before continuing.
3. Start with semantic_search query mode to pinpoint targets and leverage attachments in one call. Use include.bundle/include.lookup (or the override payloads) when you need richer context, and supply precise snippet limits so responses stay focused.
4. repository_timeline and repository_timeline_entry before planning or applying changes.
5. Keep answers tight: set INDEX_MCP_BUDGET_TOKENS or pass budgetTokens, trim maxSnippets/maxNeighbors, and prefer info or the indexing_guidance prompt for diagnostics.
6. semantic_search now defaults to returning bundle and code attachments; disable include.bundle/include.lookup when you prefer lean responses."#;

fn derive_database_name_for_status(
    ingest: &IngestResponse,
    original: Option<String>,
) -> Option<String> {
    if let Some(name) = original {
        return Some(name);
    }

    let database_path = Path::new(&ingest.database_path);
    let root_path = Path::new(&ingest.root);

    if let Ok(relative) = database_path.strip_prefix(root_path) {
        if !relative.as_os_str().is_empty() {
            return Some(relative.to_string_lossy().to_string());
        }
    }

    database_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
}

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
        name = "index_refresh",
        description = "Ingest the workspace and return index_status in a single response."
    )]
    async fn index_refresh(
        &self,
        Parameters(params): Parameters<IngestWithStatusParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.environment.update_from_meta(&ctx.meta);

        let history_limit = params.history_limit;
        let mut ingest_params = params.ingest;
        self.environment.apply_ingest_defaults(&mut ingest_params);
        let database_name_override = ingest_params.database_name.clone();

        let ingest_response = ingest_codebase(ingest_params)
            .await
            .map_err(convert_ingest_error)?;

        let database_name =
            derive_database_name_for_status(&ingest_response, database_name_override);

        let status_params = IndexStatusParams {
            root: Some(ingest_response.root.clone()),
            database_name,
            history_limit,
        };

        let status_response = get_index_status(status_params)
            .await
            .map_err(convert_index_status_error)?;

        build_ingest_with_status_result(ingest_response, status_response)
    }

    #[tool(
        name = "semantic_search",
        description = "Search indexed chunks using embeddings."
    )]
    async fn semantic_search_tool(
        &self,
        Parameters(mut request): Parameters<UnifiedSemanticSearchRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.environment.update_from_meta(&ctx.meta);
        self.environment
            .apply_semantic_defaults(&mut request.search);

        let plan = self.plan_orchestration(request);
        let handler_start = Instant::now();

        let snapshot = self.environment.snapshot();
        let recent_hits_param: Vec<SearchResultCoordinate> = snapshot
            .recent_hits
            .iter()
            .map(|hit| SearchResultCoordinate {
                path: hit.path.clone(),
                chunk_index: hit.chunk_index,
            })
            .collect();
        let filter_summary = build_search_filter_summary(&plan.search);
        let search_params = SemanticSearchParams {
            root: plan.search.root.clone(),
            query: plan.search.query.clone(),
            database_name: plan.search.database_name.clone(),
            limit: plan.search.limit,
            model: plan.search.model.clone(),
            language: plan.search.language.clone(),
            path_prefix: plan.search.path_prefix.clone(),
            path_contains: plan.search.path_contains.clone(),
            classification: plan.search.classification.clone(),
            summary_mode: plan.search.summary_mode,
            max_context_before: plan.search.max_context_before,
            max_context_after: plan.search.max_context_after,
            recent_hits: if recent_hits_param.is_empty() {
                None
            } else {
                Some(recent_hits_param)
            },
        };

        let mut response = match semantic_search(search_params).await {
            Ok(resp) => resp,
            Err(err) => {
                let elapsed_ms = handler_start.elapsed().as_secs_f64() * 1000.0;
                metrics::counter!(
                    "semantic_search_requests_total",
                    "status" => "error"
                )
                .increment(1);
                metrics::histogram!(
                    "semantic_search_handler_latency_ms",
                    "status" => "error"
                )
                .record(elapsed_ms);
                return Err(convert_semantic_search_error(err));
            }
        };

        let (deduplicated, duplicates_filtered) = self
            .environment
            .deduplicate_search_results(response.results);
        response.results = deduplicated;

        let snapshot = self.environment.snapshot();
        response.suggested_tools = build_search_suggestions(&snapshot, &response);

        let mut attachments = SearchAttachmentAccumulator::default();

        if plan.should_run_bundle() {
            let bundle_result = self.execute_bundle_attachment(&plan, &response).await;
            attachments.bundle(bundle_result);
        }

        if plan.should_run_lookup() {
            let lookup_result = self.execute_lookup_attachment(&plan, &response).await;
            attachments.lookup(lookup_result);
        }

        let mut meta =
            self.environment
                .build_search_meta(&response, duplicates_filtered, filter_summary);
        if let Some(meta_value) = attachments.attachments_meta() {
            meta.insert("attachments".to_string(), meta_value);
        }

        let summary_lines = attachments
            .bundle_summary
            .iter()
            .chain(attachments.lookup_summary.iter())
            .cloned()
            .collect::<Vec<_>>();

        let warnings_count = attachments.warnings.len();
        let bundle_outcome = attachments.bundle_outcome;
        let lookup_outcome = attachments.lookup_outcome;
        let estimated_tokens = estimate_token_cost(&response.results) as f64;
        let handler_elapsed_ms = handler_start.elapsed().as_secs_f64() * 1000.0;

        metrics::counter!(
            "semantic_search_requests_total",
            "status" => "success"
        )
        .increment(1);
        metrics::histogram!(
            "semantic_search_handler_latency_ms",
            "status" => "success"
        )
        .record(handler_elapsed_ms);
        metrics::histogram!("semantic_search_estimated_token_cost").record(estimated_tokens);
        metrics::counter!(
            "semantic_search_attachment_outcome_total",
            "attachment" => "bundle",
            "outcome" => bundle_outcome.as_label()
        )
        .increment(1);
        metrics::counter!(
            "semantic_search_attachment_outcome_total",
            "attachment" => "lookup",
            "outcome" => lookup_outcome.as_label()
        )
        .increment(1);
        if bundle_outcome.is_failure() {
            metrics::counter!(
                "semantic_search_attachment_fallback_total",
                "attachment" => "bundle"
            )
            .increment(1);
        }
        if lookup_outcome.is_failure() {
            metrics::counter!(
                "semantic_search_attachment_fallback_total",
                "attachment" => "lookup"
            )
            .increment(1);
        }
        if warnings_count > 0 {
            metrics::counter!("semantic_search_attachment_warnings_total")
                .increment(warnings_count as u64);
        }
        if duplicates_filtered > 0 {
            metrics::counter!("semantic_search_duplicates_filtered_total")
                .increment(duplicates_filtered as u64);
        }
        if let Some(diagnostics) = response.diagnostics.as_ref() {
            metrics::histogram!("semantic_search_backend_latency_ms")
                .record(diagnostics.total_latency_ms as f64);
            if let Some(embedding_latency) = diagnostics.embedding_latency_ms {
                metrics::histogram!("semantic_search_embedding_latency_ms")
                    .record(embedding_latency as f64);
            }
            if let Some(lexical_latency) = diagnostics.lexical_latency_ms {
                metrics::histogram!("semantic_search_lexical_latency_ms")
                    .record(lexical_latency as f64);
            }
        }

        let attachments_value = attachments.attachments_value();
        let warnings = attachments.warnings;

        build_semantic_search_result(response, meta, attachments_value, warnings, summary_lines)
    }

    fn plan_orchestration(&self, request: UnifiedSemanticSearchRequest) -> OrchestrationPlan {
        OrchestrationPlan {
            search: request.search,
            include_bundle: request.include.bundle || request.bundle.is_some(),
            include_lookup: request.include.lookup || request.lookup.is_some(),
            bundle_override: request.bundle,
            lookup_override: request.lookup,
            shared_budget: SharedBudget::from_spec(request.shared_budget),
        }
    }

    async fn execute_bundle_attachment(
        &self,
        plan: &OrchestrationPlan,
        search_response: &SemanticSearchResponse,
    ) -> AttachmentResult {
        let environment = self.environment.clone();
        let mut params = match plan.bundle_override.clone() {
            Some(mut params) => {
                environment.apply_bundle_defaults(&mut params);
                params
            }
            None => match derive_bundle_params_from_search(plan, search_response) {
                Ok(mut params) => {
                    environment.apply_bundle_defaults(&mut params);
                    params
                }
                Err(warning) => return AttachmentResult::Skipped { warning },
            },
        };

        if params.context_goals.is_none() && !search_response.context_goals.is_empty() {
            params.context_goals = Some(search_response.context_goals.clone());
        }
        if params.query.is_none() {
            params.query = Some(plan.search.query.clone());
        }
        let snapshot = environment.snapshot();
        if let Some(budget) = plan.shared_budget.bundle_budget_hint(&snapshot) {
            params.budget_tokens = Some(budget);
        }

        let response = match context_bundle(params.clone()).await {
            Ok(response) => response,
            Err(error) => {
                return AttachmentResult::Failed {
                    warning: format!("bundle attachment failed: {error}"),
                }
            }
        };

        let meta = environment.build_bundle_meta(&response.usage, response.usage.cache_hit);
        let attachment = match build_context_bundle_result(response.clone(), Some(meta.clone())) {
            Ok(result) => result,
            Err(error) => {
                return AttachmentResult::Failed {
                    warning: format!("bundle attachment failed to build result: {error}"),
                }
            }
        };

        let summary_line = format!("bundle attachment produced context for {}", params.file);

        AttachmentResult::Success {
            structured: attachment.structured_content.unwrap_or(Value::Null),
            meta: attachment.meta,
            summary: summary_line,
        }
    }

    async fn execute_lookup_attachment(
        &self,
        plan: &OrchestrationPlan,
        search_response: &SemanticSearchResponse,
    ) -> AttachmentResult {
        let environment = self.environment.clone();
        let mut params = plan
            .lookup_override
            .clone()
            .unwrap_or_else(|| CodeLookupParams {
                root: plan.search.root.clone(),
                database_name: plan.search.database_name.clone(),
                mode: Some("search".to_string()),
                query: Some(plan.search.query.clone()),
                file: None,
                symbol: None,
                ranges: None,
                focus_line: None,
                max_snippets: None,
                max_neighbors: None,
                budget_tokens: plan.shared_budget.lookup_tokens,
                limit: plan.search.limit,
                model: plan.search.model.clone(),
                language: plan.search.language.clone(),
                path_prefix: plan.search.path_prefix.clone(),
                path_contains: plan.search.path_contains.clone(),
                classification: plan.search.classification.clone(),
                summary_mode: plan.search.summary_mode,
                max_context_before: plan.search.max_context_before,
                max_context_after: plan.search.max_context_after,
            });

        environment.apply_code_lookup_defaults(&mut params);
        let resolved_mode = resolve_lookup_mode(&params);

        match resolved_mode.as_str() {
            "search" => {
                if params
                    .query
                    .as_ref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    return AttachmentResult::Skipped {
                        warning: "lookup attachment skipped: query required for search mode"
                            .to_string(),
                    };
                }

                let filter_summary = build_lookup_filter_summary(
                    &params.language,
                    &params.path_prefix,
                    &params.path_contains,
                    &params.classification,
                );

                let meta = environment.build_search_meta(search_response, 0, filter_summary);

                let search_clone = search_response.clone();
                let attachment = match build_code_lookup_result(
                    resolved_mode.clone(),
                    search_clone,
                    Some(meta.clone()),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        return AttachmentResult::Failed {
                            warning: format!("lookup attachment failed: {error}"),
                        }
                    }
                };

                let summary_line = format!(
                    "lookup attachment (search mode) reused {} semantic result(s).",
                    search_response.results.len()
                );

                AttachmentResult::Success {
                    structured: attachment.structured_content.unwrap_or(Value::Null),
                    meta: attachment.meta,
                    summary: summary_line,
                }
            }
            "bundle" => {
                let file = if let Some(file) = params.file.clone() {
                    file
                } else if let Some(query_file) = params.query.clone() {
                    query_file
                } else {
                    return AttachmentResult::Skipped {
                        warning: "lookup attachment skipped: bundle mode requires a file path"
                            .to_string(),
                    };
                };

                let mut bundle_params = ContextBundleParams {
                    root: params.root.clone(),
                    database_name: params.database_name.clone(),
                    file,
                    symbol: params.symbol.clone(),
                    max_snippets: params.max_snippets.or(params.limit),
                    max_neighbors: params.max_neighbors,
                    budget_tokens: params.budget_tokens,
                    ranges: params.ranges.clone(),
                    focus_line: params.focus_line,
                    query: None,
                    context_goals: if search_response.context_goals.is_empty() {
                        None
                    } else {
                        Some(search_response.context_goals.clone())
                    },
                };

                environment.apply_bundle_defaults(&mut bundle_params);
                if bundle_params.budget_tokens.is_none() {
                    let snapshot = environment.snapshot();
                    if let Some(budget) = plan.shared_budget.bundle_budget_hint(&snapshot) {
                        bundle_params.budget_tokens = Some(budget);
                    }
                }
                if bundle_params.context_goals.is_none()
                    && !search_response.context_goals.is_empty()
                {
                    bundle_params.context_goals = Some(search_response.context_goals.clone());
                }
                if bundle_params.query.is_none() {
                    bundle_params.query = Some(plan.search.query.clone());
                }

                let response = match context_bundle(bundle_params.clone()).await {
                    Ok(response) => response,
                    Err(error) => {
                        return AttachmentResult::Failed {
                            warning: format!("lookup bundle attachment failed: {error}"),
                        }
                    }
                };

                let meta = environment.build_bundle_meta(&response.usage, response.usage.cache_hit);

                let attachment = match build_code_lookup_bundle_response(
                    resolved_mode.clone(),
                    response.clone(),
                    Some(meta.clone()),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        return AttachmentResult::Failed {
                            warning: format!(
                                "lookup bundle attachment failed to build result: {error}"
                            ),
                        }
                    }
                };

                let summary_line = format!(
                    "lookup attachment (bundle mode) produced context for {}",
                    bundle_params.file
                );

                AttachmentResult::Success {
                    structured: attachment.structured_content.unwrap_or(Value::Null),
                    meta: attachment.meta,
                    summary: summary_line,
                }
            }
            _ => AttachmentResult::Skipped {
                warning: format!(
                    "lookup attachment skipped: mode '{resolved_mode}' is not supported."
                ),
            },
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
    let value = compact::ingest(response).map_err(|error| {
        McpError::internal_error(format!("Failed to compact ingest result: {error}"), None)
    })?;

    Ok(CallToolResult {
        content: vec![Content::text(summary)],
        structured_content: Some(value),
        is_error: Some(false),
        meta: None,
    })
}

fn build_ingest_with_status_result(
    ingest: IngestResponse,
    status: IndexStatusResponse,
) -> Result<CallToolResult, McpError> {
    let summary = format!(
        "{} {}",
        summarize_ingest(&ingest),
        summarize_index_status(&status)
    );
    let value = compact::ingest_with_status(ingest, status).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact ingest+status result: {error}"),
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
    let value = compact::index_status(response).map_err(|error| {
        McpError::internal_error(format!("Failed to compact status result: {error}"), None)
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
    mut meta: Meta,
    attachments: Option<Value>,
    warnings: Vec<String>,
    attachment_summaries: Vec<String>,
) -> Result<CallToolResult, McpError> {
    let mut summary_lines = vec![summarize_semantic_search(&response)];
    summary_lines.extend(attachment_summaries);
    if !warnings.is_empty() {
        summary_lines.push(format!("Warnings: {}", warnings.join("; ")));
    }
    let summary = summary_lines.join("\n");

    let mut value = compact::semantic_search(response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact semantic search result: {error}"),
            None,
        )
    })?;

    if let Value::Object(ref mut map) = value {
        if let Some(attachments) = attachments {
            map.insert("att".to_string(), attachments);
        }
        if !warnings.is_empty() {
            map.insert(
                "warn".to_string(),
                Value::Array(warnings.into_iter().map(Value::String).collect()),
            );
        }
    }

    if let Some(Value::Object(att_meta)) = meta.get("attachments") {
        if att_meta.is_empty() {
            meta.remove("attachments");
        }
    }

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
    let value = compact::code_lookup_search(mode, search_result).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact code_lookup search result: {error}"),
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
    let value = compact::code_lookup_bundle(mode, bundle).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact code_lookup bundle result: {error}"),
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
        if let Some(metadata) = summarize_definition_metadata(focus) {
            parts.push(metadata);
        }
    } else if let Some(primary) = bundle.definitions.first() {
        parts.push(format!(
            "Primary definition {} {}.",
            primary.kind, primary.name
        ));
        if let Some(metadata) = summarize_definition_metadata(primary) {
            parts.push(metadata);
        }
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

fn summarize_definition_metadata(definition: &BundleDefinition) -> Option<String> {
    let mut segments = Vec::new();

    if let Some(visibility) = definition
        .visibility
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        segments.push(format!("visibility {}", visibility.trim()));
    }

    if let Some(docstring) = definition.docstring.as_deref() {
        if let Some(line) = docstring
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
        {
            segments.push(format!("doc {}", truncate_summary_line(line, 96)));
        }
    }

    if segments.is_empty() {
        None
    } else {
        Some(format!("Details: {}.", segments.join("; ")))
    }
}

fn truncate_summary_line(line: &str, max_len: usize) -> String {
    if line.chars().count() <= max_len {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(max_len).collect();
    while truncated
        .chars()
        .last()
        .is_some_and(|ch| ch.is_whitespace())
    {
        truncated.pop();
    }
    truncated.push_str("...");
    truncated
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

    let value = compact::context_bundle(response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact context bundle result: {error}"),
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

    let value = compact::repository_timeline(response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact repository timeline result: {error}"),
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

    let value = compact::repository_timeline_entry(response).map_err(|error| {
        McpError::internal_error(
            format!("Failed to compact repository timeline entry result: {error}"),
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

fn derive_bundle_params_from_search(
    plan: &OrchestrationPlan,
    search_response: &SemanticSearchResponse,
) -> Result<ContextBundleParams, String> {
    let top = search_response.results.first().ok_or_else(|| {
        "bundle attachment skipped: semantic search returned no results".to_string()
    })?;

    let focus_line = top
        .line_start
        .and_then(|line| if line > 0 { Some(line as u32) } else { None });

    let range = match (top.line_start, top.line_end) {
        (Some(start), Some(end)) if start >= 0 && end >= 0 => {
            let start = start as u32;
            let end = end as u32;
            if start <= end {
                Some(LineRange {
                    start_line: start,
                    end_line: end,
                })
            } else {
                Some(LineRange {
                    start_line: end,
                    end_line: start,
                })
            }
        }
        _ => None,
    };

    Ok(ContextBundleParams {
        root: plan.search.root.clone(),
        database_name: plan.search.database_name.clone(),
        file: top.path.clone(),
        symbol: None,
        max_snippets: None,
        max_neighbors: Some(6),
        budget_tokens: plan.shared_budget.bundle_tokens,
        ranges: range.map(|r| vec![r]),
        focus_line,
        query: None,
        context_goals: if search_response.context_goals.is_empty() {
            None
        } else {
            Some(search_response.context_goals.clone())
        },
    })
}

fn resolve_lookup_mode(params: &CodeLookupParams) -> String {
    if let Some(mode) = &params.mode {
        return mode.clone();
    }
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
}

mod compact {
    use super::*;
    use serde::Serialize;
    use serde_json::Value;

    pub(super) fn ingest(response: IngestResponse) -> serde_json::Result<Value> {
        serde_json::to_value(CompactIngestResponse::from(response))
    }

    pub(super) fn ingest_with_status(
        ingest: IngestResponse,
        status: IndexStatusResponse,
    ) -> serde_json::Result<Value> {
        serde_json::to_value(CompactIngestWithStatusResponse::new(ingest, status))
    }

    pub(super) fn index_status(response: IndexStatusResponse) -> serde_json::Result<Value> {
        serde_json::to_value(CompactIndexStatusResponse::from(response))
    }

    pub(super) fn semantic_search(response: SemanticSearchResponse) -> serde_json::Result<Value> {
        serde_json::to_value(CompactSemanticSearchResponse::from(response))
    }

    pub(super) fn code_lookup_search(
        mode: String,
        search: SemanticSearchResponse,
    ) -> serde_json::Result<Value> {
        serde_json::to_value(CompactCodeLookupResponse::search(mode, search))
    }

    pub(super) fn code_lookup_bundle(
        mode: String,
        bundle: ContextBundleResponse,
    ) -> serde_json::Result<Value> {
        serde_json::to_value(CompactCodeLookupResponse::bundle(mode, bundle))
    }

    pub(super) fn context_bundle(response: ContextBundleResponse) -> serde_json::Result<Value> {
        serde_json::to_value(CompactContextBundleResponse::from(response))
    }

    pub(super) fn repository_timeline(
        response: RepositoryTimelineResponse,
    ) -> serde_json::Result<Value> {
        serde_json::to_value(CompactRepositoryTimelineResponse::from(response))
    }

    pub(super) fn repository_timeline_entry(
        response: RepositoryTimelineEntryLookupResponse,
    ) -> serde_json::Result<Value> {
        serde_json::to_value(CompactRepositoryTimelineEntryResponse::from(response))
    }

    #[derive(Serialize)]
    struct CompactIngestResponse {
        t: &'static str,
        r: String,
        db: String,
        sz: u64,
        fc: usize,
        ec: usize,
        gc: CompactGraphCounts,
        du: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        mdl: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mb: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        md: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ml: Option<u128>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mq: Option<bool>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        sk: Vec<CompactSkippedFile>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        del: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ev: Option<CompactEviction>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rf: Option<usize>,
    }

    impl From<IngestResponse> for CompactIngestResponse {
        fn from(response: IngestResponse) -> Self {
            Self {
                t: "ingest",
                r: response.root,
                db: response.database_path,
                sz: response.database_size_bytes,
                fc: response.ingested_file_count,
                ec: response.embedded_chunk_count,
                gc: CompactGraphCounts {
                    n: response.graph_node_count as u64,
                    e: response.graph_edge_count as u64,
                },
                du: response.duration_ms,
                mdl: response.embedding_model,
                mb: response.embedding_backend,
                md: response.embedding_dimension,
                ml: response.embedding_latency_ms,
                mq: response.embedding_quantized,
                sk: response
                    .skipped
                    .into_iter()
                    .map(CompactSkippedFile::from)
                    .collect(),
                del: response.deleted_paths,
                ev: response.evicted.map(CompactEviction::from),
                rf: response.reused_file_count,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactIngestWithStatusResponse {
        t: &'static str,
        ing: CompactIngestResponse,
        st: CompactIndexStatusResponse,
    }

    impl CompactIngestWithStatusResponse {
        fn new(ingest: IngestResponse, status: IndexStatusResponse) -> Self {
            Self {
                t: "refresh",
                ing: CompactIngestResponse::from(ingest),
                st: CompactIndexStatusResponse::from(status),
            }
        }
    }

    #[derive(Serialize)]
    struct CompactGraphCounts {
        n: u64,
        e: u64,
    }

    #[derive(Serialize)]
    struct CompactSkippedFile {
        p: String,
        why: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sz: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        msg: Option<String>,
    }

    impl From<SkippedFile> for CompactSkippedFile {
        fn from(file: SkippedFile) -> Self {
            Self {
                p: file.path,
                why: file.reason,
                sz: file.size,
                msg: file.message,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactEviction {
        db: String,
        sb: u64,
        sa: u64,
        ec: usize,
        en: usize,
    }

    impl From<EvictionReport> for CompactEviction {
        fn from(report: EvictionReport) -> Self {
            Self {
                db: report.database_path,
                sb: report.size_before,
                sa: report.size_after,
                ec: report.evicted_chunks,
                en: report.evicted_nodes,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactIndexStatusResponse {
        t: &'static str,
        db: String,
        exists: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        sz: Option<u64>,
        tf: u64,
        tc: u64,
        gc: CompactGraphCounts,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        mdl: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        latest: Option<CompactIndexStatusIngestion>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        recent: Vec<CompactIndexStatusIngestion>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        curr_sha: Option<String>,
        stale: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        indexed_at: Option<i64>,
    }

    impl From<IndexStatusResponse> for CompactIndexStatusResponse {
        fn from(response: IndexStatusResponse) -> Self {
            Self {
                t: "status",
                db: response.database_path,
                exists: response.database_exists,
                sz: response.database_size_bytes,
                tf: response.total_files,
                tc: response.total_chunks,
                gc: CompactGraphCounts {
                    n: response.total_graph_nodes,
                    e: response.total_graph_edges,
                },
                mdl: response.embedding_models,
                latest: response
                    .latest_ingestion
                    .map(CompactIndexStatusIngestion::from),
                recent: response
                    .recent_ingestions
                    .into_iter()
                    .map(CompactIndexStatusIngestion::from)
                    .collect(),
                sha: response.commit_sha,
                curr_sha: response.current_commit_sha,
                stale: response.is_stale,
                indexed_at: response.indexed_at,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactIndexStatusIngestion {
        id: String,
        r: String,
        start: i64,
        end: i64,
        dur: i64,
        fc: i64,
        sk: i64,
        del: i64,
    }

    impl From<IndexStatusIngestion> for CompactIndexStatusIngestion {
        fn from(ingestion: IndexStatusIngestion) -> Self {
            Self {
                id: ingestion.id,
                r: ingestion.root,
                start: ingestion.started_at,
                end: ingestion.finished_at,
                dur: ingestion.duration_ms,
                fc: ingestion.file_count,
                sk: ingestion.skipped_count,
                del: ingestion.deleted_count,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactSemanticSearchResponse {
        t: &'static str,
        db: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mdl: Option<String>,
        tc: u64,
        ec: u64,
        r: Vec<CompactSemanticMatch>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        sg: Vec<CompactSuggestedTool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diag: Option<CompactSearchDiagnostics>,
        #[serde(skip_serializing_if = "Option::is_none")]
        intent: Option<QueryIntent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        conf: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        goals: Option<Vec<ContextGoal>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        clar: Vec<String>,
    }

    impl From<SemanticSearchResponse> for CompactSemanticSearchResponse {
        fn from(response: SemanticSearchResponse) -> Self {
            Self {
                t: "sem",
                db: response.database_path,
                name: response.database_name,
                mdl: response.embedding_model,
                tc: response.total_chunks,
                ec: response.evaluated_chunks,
                r: response
                    .results
                    .into_iter()
                    .map(CompactSemanticMatch::from)
                    .collect(),
                sg: response
                    .suggested_tools
                    .into_iter()
                    .map(CompactSuggestedTool::from)
                    .collect(),
                diag: response.diagnostics.map(CompactSearchDiagnostics::from),
                intent: response.query_intent,
                conf: response.intent_confidence,
                goals: if response.context_goals.is_empty() {
                    None
                } else {
                    Some(response.context_goals)
                },
                clar: response.clarification_prompts,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactSemanticMatch {
        p: String,
        ci: i32,
        ns: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        sc: Option<f32>,
        cl: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ls: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        le: Option<i64>,
        src: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cb: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ca: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sum: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sym: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ident: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        st: Option<String>,
        src_kind: &'static str,
        cf: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    }

    impl From<SemanticSearchMatch> for CompactSemanticMatch {
        fn from(result: SemanticSearchMatch) -> Self {
            Self {
                p: result.path,
                ci: result.chunk_index,
                ns: result.normalized_score,
                sc: Some(result.score),
                cl: classification_name(result.classification),
                lang: result.language,
                ls: result.line_start,
                le: result.line_end,
                src: result.content,
                cb: result.context_before,
                ca: result.context_after,
                sum: result.summary,
                sym: result.symbol,
                ident: result.identifier,
                st: result.source_type,
                src_kind: search_source_name(result.source),
                cf: result.confidence,
                meta: result.metadata,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactSuggestedTool {
        id: String,
        r: u32,
        s: f32,
        #[serde(skip_serializing_if = "Option::is_none")]
        d: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pv: Option<String>,
        params: Value,
    }

    impl From<SuggestedTool> for CompactSuggestedTool {
        fn from(tool: SuggestedTool) -> Self {
            Self {
                id: tool.tool,
                r: tool.rank,
                s: tool.score,
                d: tool.description,
                pv: tool.preview,
                params: tool.parameters,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactSearchDiagnostics {
        #[serde(skip_serializing_if = "Option::is_none")]
        mdl: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        be: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        q: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dim: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        emb_ms: Option<u128>,
        #[serde(skip_serializing_if = "Option::is_none")]
        lex_ms: Option<u128>,
        tot_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        eval: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        intent: Option<QueryIntent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        conf: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        goals: Option<Vec<ContextGoal>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        lex_lim: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        emb_lim: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ann: Option<u32>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        clar: Vec<String>,
    }

    impl From<SearchDiagnostics> for CompactSearchDiagnostics {
        fn from(diag: SearchDiagnostics) -> Self {
            let SearchDiagnostics {
                model,
                backend,
                quantized,
                dimension,
                embedding_latency_ms,
                lexical_latency_ms,
                total_latency_ms,
                evaluated_chunk_count,
                query_intent,
                intent_confidence,
                context_goals,
                lexical_limit,
                embedding_limit,
                ann_candidate_count,
                clarification_reasons,
            } = diag;

            Self {
                mdl: model,
                be: backend,
                q: quantized,
                dim: dimension,
                emb_ms: embedding_latency_ms,
                lex_ms: lexical_latency_ms,
                tot_ms: total_latency_ms,
                eval: evaluated_chunk_count,
                intent: query_intent,
                conf: intent_confidence,
                goals: if context_goals.is_empty() {
                    None
                } else {
                    Some(context_goals)
                },
                lex_lim: lexical_limit,
                emb_lim: embedding_limit,
                ann: ann_candidate_count,
                clar: clarification_reasons,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactCodeLookupResponse {
        t: &'static str,
        mode: String,
        sem: Option<CompactSemanticSearchResponse>,
        bundle: Option<CompactContextBundleResponse>,
    }

    impl CompactCodeLookupResponse {
        fn search(mode: String, search: SemanticSearchResponse) -> Self {
            Self {
                t: "code",
                mode,
                sem: Some(CompactSemanticSearchResponse::from(search)),
                bundle: None,
            }
        }

        fn bundle(mode: String, bundle: ContextBundleResponse) -> Self {
            Self {
                t: "code",
                mode,
                sem: None,
                bundle: Some(CompactContextBundleResponse::from(bundle)),
            }
        }
    }

    #[derive(Serialize)]
    struct CompactContextBundleResponse {
        t: &'static str,
        db: String,
        file: CompactBundleFile,
        defs: Vec<CompactBundleDefinition>,
        #[serde(skip_serializing_if = "Option::is_none")]
        focus: Option<CompactBundleDefinition>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        rel: Vec<CompactBundleNeighbor>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        sn: Vec<CompactBundleSnippet>,
        #[serde(skip_serializing_if = "Option::is_none")]
        latest: Option<CompactBundleIngestion>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        warn: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        quick: Vec<CompactQuickLink>,
        use_stats: CompactBundleUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        diag: Option<CompactBundleDiagnostics>,
    }

    impl From<ContextBundleResponse> for CompactContextBundleResponse {
        fn from(response: ContextBundleResponse) -> Self {
            Self {
                t: "ctx",
                db: response.database_path,
                file: CompactBundleFile::from(response.file),
                defs: response
                    .definitions
                    .into_iter()
                    .map(CompactBundleDefinition::from)
                    .collect(),
                focus: response.focus_definition.map(CompactBundleDefinition::from),
                rel: response
                    .related
                    .into_iter()
                    .map(CompactBundleNeighbor::from)
                    .collect(),
                sn: response
                    .snippets
                    .into_iter()
                    .map(CompactBundleSnippet::from)
                    .collect(),
                latest: response.latest_ingestion.map(CompactBundleIngestion::from),
                warn: response.warnings,
                quick: response
                    .quick_links
                    .into_iter()
                    .map(CompactQuickLink::from)
                    .collect(),
                use_stats: CompactBundleUsage::from(response.usage),
                diag: response.diagnostics.map(CompactBundleDiagnostics::from),
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleFile {
        p: String,
        sz: i64,
        m: i64,
        hash: String,
        idx: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
    }

    impl From<BundleFileMetadata> for CompactBundleFile {
        fn from(file: BundleFileMetadata) -> Self {
            Self {
                p: file.path,
                sz: file.size,
                m: file.modified,
                hash: file.hash,
                idx: file.last_indexed_at,
                brief: file.brief,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleDefinition {
        id: String,
        name: String,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rs: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        re: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        vis: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        doc: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        todo: Option<u32>,
    }

    impl From<BundleDefinition> for CompactBundleDefinition {
        fn from(def: BundleDefinition) -> Self {
            Self {
                id: def.id,
                name: def.name,
                kind: def.kind,
                sig: def.signature,
                rs: def.range_start,
                re: def.range_end,
                meta: def.metadata,
                vis: def.visibility,
                doc: def.docstring,
                todo: def.todo_count,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleNeighbor {
        id: String,
        ty: String,
        dir: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        sp: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tp: Option<String>,
        node: CompactNeighborNode,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    }

    impl From<BundleEdgeNeighbor> for CompactBundleNeighbor {
        fn from(neighbor: BundleEdgeNeighbor) -> Self {
            Self {
                id: neighbor.id,
                ty: neighbor.r#type,
                dir: neighbor_direction_name(neighbor.direction),
                sp: neighbor.source_path,
                tp: neighbor.target_path,
                node: CompactNeighborNode::from(neighbor.neighbor),
                meta: neighbor.metadata,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactNeighborNode {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        p: Option<String>,
        kind: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rs: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        re: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    }

    impl From<NeighborNode> for CompactNeighborNode {
        fn from(node: NeighborNode) -> Self {
            Self {
                id: node.id,
                p: node.path,
                kind: node.kind,
                name: node.name,
                sig: node.signature,
                rs: node.range_start,
                re: node.range_end,
                meta: node.metadata,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleSnippet {
        #[serde(skip_serializing_if = "Option::is_none")]
        p: Option<String>,
        src: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        ci: Option<i32>,
        txt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ls: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        le: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sum: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sym: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ident: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        st: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sc: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sim: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mdl: Option<String>,
    }

    impl From<BundleSnippet> for CompactBundleSnippet {
        fn from(snippet: BundleSnippet) -> Self {
            Self {
                p: snippet.path,
                src: snippet_source_name(snippet.source),
                ci: snippet.chunk_index,
                txt: snippet.content,
                ls: snippet.line_start,
                le: snippet.line_end,
                sum: snippet.summary,
                sym: snippet.symbol,
                ident: snippet.identifier,
                st: snippet.source_type,
                meta: snippet.metadata,
                sc: snippet.score,
                sim: snippet.similarity,
                mdl: snippet.embedding_model,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleIngestion {
        id: String,
        ts: i64,
        dur: i64,
        fc: i64,
    }

    impl From<BundleIngestionSummary> for CompactBundleIngestion {
        fn from(summary: BundleIngestionSummary) -> Self {
            Self {
                id: summary.id,
                ts: summary.finished_at,
                dur: summary.duration_ms,
                fc: summary.file_count,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactQuickLink {
        ty: &'static str,
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dir: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sym: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    }

    impl From<ContextBundleQuickLink> for CompactQuickLink {
        fn from(link: ContextBundleQuickLink) -> Self {
            Self {
                ty: quick_link_type_name(link.r#type),
                label: link.label,
                path: link.path,
                dir: link.direction.map(neighbor_direction_name),
                sym: link.symbol_id,
                kind: link.symbol_kind,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleUsage {
        def: usize,
        sn: usize,
        used: usize,
        bud: usize,
        rem: usize,
        omit: usize,
        exc: usize,
        sum: usize,
        cache: bool,
    }

    impl From<BundleUsageStats> for CompactBundleUsage {
        fn from(stats: BundleUsageStats) -> Self {
            Self {
                def: stats.definitions_tokens,
                sn: stats.snippet_tokens,
                used: stats.used_tokens,
                bud: stats.budget_tokens,
                rem: stats.remaining_tokens,
                omit: stats.omitted_snippets,
                exc: stats.excerpt_snippets,
                sum: stats.summary_snippets,
                cache: stats.cache_hit,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactBundleDiagnostics {
        #[serde(skip_serializing_if = "Option::is_none")]
        q: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mdl: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        be: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ms: Option<u128>,
        #[serde(skip_serializing_if = "Option::is_none")]
        smin: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        smax: Option<f32>,
    }

    impl From<BundleDiagnostics> for CompactBundleDiagnostics {
        fn from(d: BundleDiagnostics) -> Self {
            Self {
                q: d.query,
                mdl: d.embedding_model,
                be: d.embedding_backend,
                ms: d.embedding_latency_ms,
                smin: d.similarity_min,
                smax: d.similarity_max,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactRepositoryTimelineResponse {
        t: &'static str,
        root: String,
        br: String,
        limit: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<String>,
        merges: bool,
        stats: bool,
        diffs: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        total: usize,
        merges_count: usize,
        ins: i64,
        del: i64,
        entries: Vec<CompactTimelineEntry>,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        db: Option<String>,
    }

    impl From<RepositoryTimelineResponse> for CompactRepositoryTimelineResponse {
        fn from(response: RepositoryTimelineResponse) -> Self {
            Self {
                t: "timeline",
                root: response.repository_root,
                br: response.branch,
                limit: response.limit,
                since: response.since,
                merges: response.include_merges,
                stats: response.include_file_stats,
                diffs: response.include_diffs,
                paths: response.paths,
                diff: response.diff_pattern,
                total: response.total_commits,
                merges_count: response.merge_commits,
                ins: response.total_insertions,
                del: response.total_deletions,
                entries: response
                    .entries
                    .into_iter()
                    .map(CompactTimelineEntry::from)
                    .collect(),
                remote: response.remote_url,
                db: response.database_path,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactTimelineEntry {
        sha: String,
        subj: String,
        sum: String,
        auth: CompactIdentity,
        auth_ts: String,
        comm: CompactIdentity,
        comm_ts: String,
        parents: Vec<String>,
        merge: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pr: Option<i64>,
        files: usize,
        ins: i64,
        del: i64,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        changes: Vec<CompactFileChange>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff_preview: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff_ptr: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        top: Vec<CompactTopFile>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        churn: Vec<CompactDirectoryChurn>,
        diff_stats: CompactDiffSummary,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        highlights: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pr_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        captured: Option<i64>,
    }

    impl From<RepositoryTimelineEntry> for CompactTimelineEntry {
        fn from(entry: RepositoryTimelineEntry) -> Self {
            Self {
                sha: entry.sha,
                subj: entry.subject,
                sum: entry.summary,
                auth: CompactIdentity::from(entry.author),
                auth_ts: entry.author_date,
                comm: CompactIdentity::from(entry.committer),
                comm_ts: entry.committer_date,
                parents: entry.parents,
                merge: entry.is_merge,
                pr: entry.pull_request_number,
                files: entry.files_changed,
                ins: entry.insertions,
                del: entry.deletions,
                changes: entry
                    .file_changes
                    .into_iter()
                    .map(CompactFileChange::from)
                    .collect(),
                diff: entry.diff,
                diff_preview: entry.diff_preview,
                diff_ptr: entry.diff_pointer,
                top: entry
                    .top_files
                    .into_iter()
                    .map(CompactTopFile::from)
                    .collect(),
                churn: entry
                    .directory_churn
                    .into_iter()
                    .map(CompactDirectoryChurn::from)
                    .collect(),
                diff_stats: CompactDiffSummary::from(entry.diff_summary),
                highlights: entry.highlights,
                pr_url: entry.pull_request_url,
                captured: entry.captured_at,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactIdentity {
        name: String,
        email: String,
    }

    impl From<TimelineIdentity> for CompactIdentity {
        fn from(identity: TimelineIdentity) -> Self {
            Self {
                name: identity.name,
                email: identity.email,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactFileChange {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        ins: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        del: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        net: Option<i64>,
    }

    impl From<RepositoryTimelineFileChange> for CompactFileChange {
        fn from(change: RepositoryTimelineFileChange) -> Self {
            Self {
                path: change.path,
                ins: change.insertions,
                del: change.deletions,
                net: change.net,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactTopFile {
        path: String,
        ins: i64,
        del: i64,
        net: i64,
    }

    impl From<RepositoryTimelineTopFile> for CompactTopFile {
        fn from(file: RepositoryTimelineTopFile) -> Self {
            Self {
                path: file.path,
                ins: file.insertions,
                del: file.deletions,
                net: file.net,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactDirectoryChurn {
        path: String,
        ins: i64,
        del: i64,
        net: i64,
        files: usize,
    }

    impl From<RepositoryTimelineDirectoryChurn> for CompactDirectoryChurn {
        fn from(churn: RepositoryTimelineDirectoryChurn) -> Self {
            Self {
                path: churn.path,
                ins: churn.insertions,
                del: churn.deletions,
                net: churn.net,
                files: churn.files_changed,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactDiffSummary {
        files: usize,
        ins: i64,
        del: i64,
        net: i64,
    }

    impl From<RepositoryTimelineDiffSummary> for CompactDiffSummary {
        fn from(summary: RepositoryTimelineDiffSummary) -> Self {
            Self {
                files: summary.files_changed,
                ins: summary.insertions,
                del: summary.deletions,
                net: summary.net,
            }
        }
    }

    #[derive(Serialize)]
    struct CompactRepositoryTimelineEntryResponse {
        t: &'static str,
        db: String,
        entry: CompactTimelineEntry,
        #[serde(skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    }

    impl From<RepositoryTimelineEntryLookupResponse> for CompactRepositoryTimelineEntryResponse {
        fn from(response: RepositoryTimelineEntryLookupResponse) -> Self {
            Self {
                t: "timeline_entry",
                db: response.database_path,
                entry: CompactTimelineEntry::from(response.entry),
                diff: response.diff,
            }
        }
    }

    fn classification_name(classification: Classification) -> &'static str {
        match classification {
            Classification::Function => "function",
            Classification::Comment => "comment",
            Classification::Code => "code",
        }
    }

    fn search_source_name(source: SearchSource) -> &'static str {
        match source {
            SearchSource::Embedding => "embedding",
            SearchSource::Lexical => "lexical",
        }
    }

    fn neighbor_direction_name(direction: NeighborDirection) -> &'static str {
        match direction {
            NeighborDirection::Incoming => "in",
            NeighborDirection::Outgoing => "out",
        }
    }

    fn snippet_source_name(source: SnippetSource) -> &'static str {
        match source {
            SnippetSource::Chunk => "chunk",
            SnippetSource::Content => "content",
        }
    }

    fn quick_link_type_name(kind: QuickLinkType) -> &'static str {
        match kind {
            QuickLinkType::File => "file",
            QuickLinkType::RelatedSymbol => "symbol",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{
        BundleDefinition, BundleFileMetadata, BundleSnippet, BundleUsageStats,
        ContextBundleQuickLink, ContextBundleResponse, QuickLinkType, SnippetSource,
    };
    use crate::index_status::{IndexStatusIngestion, IndexStatusResponse};
    use crate::ingest::IngestResponse;
    use crate::search::{
        Classification, SearchDiagnostics, SearchSource, SemanticSearchMatch,
        SemanticSearchResponse,
    };
    use serde_json::json;

    fn ingest_response_with_paths(root: &str, database_path: &str) -> IngestResponse {
        IngestResponse {
            root: root.into(),
            database_path: database_path.into(),
            database_size_bytes: 0,
            ingested_file_count: 0,
            skipped: Vec::new(),
            deleted_paths: Vec::new(),
            duration_ms: 0,
            embedded_chunk_count: 0,
            embedding_model: None,
            embedding_backend: None,
            embedding_dimension: None,
            embedding_latency_ms: None,
            embedding_quantized: None,
            graph_node_count: 0,
            graph_edge_count: 0,
            evicted: None,
            reused_file_count: None,
        }
    }

    fn sample_match(path: &str, chunk_index: i32) -> SemanticSearchMatch {
        SemanticSearchMatch {
            path: path.to_string(),
            chunk_index,
            score: 0.9,
            normalized_score: 0.9,
            language: Some("Rust".to_string()),
            classification: Classification::Function,
            content: format!("fn sample_{chunk_index}() {{}}"),
            embedding_model: "mock".to_string(),
            byte_start: Some(0),
            byte_end: Some(16),
            line_start: Some(1),
            line_end: Some(1),
            context_before: None,
            context_after: None,
            source: SearchSource::Embedding,
            summary: None,
            symbol: None,
            identifier: None,
            source_type: None,
            metadata: None,
            confidence: 0.9,
        }
    }

    fn sample_bundle_response() -> ContextBundleResponse {
        ContextBundleResponse {
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
                path: None,
                source: SnippetSource::Chunk,
                chunk_index: Some(0),
                content: "fn foo() {}".into(),
                byte_start: Some(0),
                byte_end: Some(12),
                line_start: Some(1),
                line_end: Some(1),
                served_count: None,
                summary: None,
                symbol: None,
                identifier: None,
                source_type: None,
                metadata: None,
                score: None,
                similarity: None,
                embedding_model: None,
                embedding: None,
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
            usage: BundleUsageStats {
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
            diagnostics: None,
        }
    }

    fn sample_semantic_response() -> SemanticSearchResponse {
        SemanticSearchResponse {
            database_path: "db.sqlite".into(),
            database_name: Some("db.sqlite".into()),
            embedding_model: Some("custom-model".into()),
            total_chunks: 200,
            evaluated_chunks: 150,
            results: vec![SemanticSearchMatch {
                path: "src/main.rs".into(),
                chunk_index: 0,
                score: 1.0,
                normalized_score: 0.92,
                language: Some("Rust".into()),
                classification: Classification::Function,
                content: "fn example() {}".into(),
                embedding_model: "custom-model".into(),
                byte_start: Some(10),
                byte_end: Some(20),
                line_start: Some(44),
                line_end: Some(47),
                context_before: None,
                context_after: None,
                source: SearchSource::Lexical,
                summary: None,
                symbol: None,
                identifier: None,
                source_type: None,
                metadata: None,
                confidence: 0.92,
            }],
            summary_mode: SummaryMode::Brief,
            query_intent: Some(QueryIntent::Lexical),
            intent_confidence: Some(0.8),
            context_goals: vec![ContextGoal::GeneralUnderstanding],
            clarification_prompts: Vec::new(),
            suggested_tools: Vec::new(),
            diagnostics: Some(SearchDiagnostics {
                model: Some("custom-model".into()),
                lexical_latency_ms: Some(5),
                total_latency_ms: 25,
                evaluated_chunk_count: Some(150),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn environment_defaults_fill_missing_semantic_fields() {
        let env = EnvironmentState::new();
        let mut meta = Meta::new();
        meta.insert("cwd".to_string(), json!("/workspace"));
        meta.insert(
            "tokenUsage".to_string(),
            json!({ "remainingContextTokens": 2048 }),
        );
        env.update_from_meta(&meta);

        let mut request = SemanticSearchRequest {
            root: None,
            query: "alpha".to_string(),
            database_name: None,
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

        env.apply_semantic_defaults(&mut request);

        assert_eq!(request.root.as_deref(), Some("/workspace"));
        assert_eq!(request.limit, Some(DEFAULT_SEARCH_LIMIT_HINT));
        assert_eq!(request.summary_mode, Some(SummaryMode::Brief));
        assert_eq!(request.max_context_before, Some(1));
        assert_eq!(request.max_context_after, Some(1));
    }

    #[test]
    fn deduplicate_search_results_tracks_history() {
        let env = EnvironmentState::new();
        let first_batch = vec![
            sample_match("src/lib.rs", 0),
            sample_match("src/lib.rs", 0),
            sample_match("src/lib.rs", 1),
        ];
        let (retained, duplicates) = env.deduplicate_search_results(first_batch);
        assert_eq!(retained.len(), 2);
        assert_eq!(duplicates, 1);

        let second_batch = vec![
            sample_match("src/lib.rs", 0),
            sample_match("src/other.rs", 0),
        ];
        let (retained_again, duplicates_again) = env.deduplicate_search_results(second_batch);
        assert_eq!(retained_again.len(), 1);
        assert_eq!(retained_again[0].path, "src/other.rs");
        assert_eq!(duplicates_again, 1);
    }

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
            embedding_backend: Some("onnx".into()),
            embedding_dimension: Some(384),
            embedding_latency_ms: Some(250),
            embedding_quantized: Some(false),
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
    fn build_ingest_with_status_result_compacts_payload() {
        let ingest_response = IngestResponse {
            root: "/workspace".into(),
            database_path: "/workspace/.mcp-index.sqlite".into(),
            database_size_bytes: 2_048,
            ingested_file_count: 5,
            skipped: Vec::new(),
            deleted_paths: Vec::new(),
            duration_ms: 2_000,
            embedded_chunk_count: 128,
            embedding_model: None,
            embedding_backend: None,
            embedding_dimension: None,
            embedding_latency_ms: None,
            embedding_quantized: None,
            graph_node_count: 1,
            graph_edge_count: 2,
            evicted: None,
            reused_file_count: None,
        };

        let latest = IndexStatusIngestion {
            id: "ingest-2".into(),
            root: "/workspace".into(),
            started_at: 10,
            finished_at: 20,
            duration_ms: 1_000,
            file_count: 5,
            skipped_count: 0,
            deleted_count: 0,
        };

        let status_response = IndexStatusResponse {
            database_path: "/workspace/.mcp-index.sqlite".into(),
            database_exists: true,
            database_size_bytes: Some(2_048),
            total_files: 5,
            total_chunks: 128,
            embedding_models: vec!["model-A".into()],
            total_graph_nodes: 1,
            total_graph_edges: 2,
            latest_ingestion: Some(latest.clone()),
            recent_ingestions: vec![latest],
            commit_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            indexed_at: Some(20),
            current_commit_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            is_stale: false,
        };

        let result = build_ingest_with_status_result(ingest_response, status_response)
            .expect("combined result");

        let summary_text = result
            .content
            .first()
            .and_then(|content| content.raw.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_else(|| panic!("expected text content, found {:?}", result.content.first()));
        assert!(summary_text.contains("Indexed 5 file(s)"));
        assert!(summary_text.contains("Database /workspace/.mcp-index.sqlite tracks 5 file(s)"));

        let structured = result
            .structured_content
            .expect("structured content available");
        let object = structured.as_object().expect("object payload");
        assert_eq!(
            object.get("t").and_then(|value| value.as_str()),
            Some("refresh")
        );
        assert!(object.contains_key("ing"));
        assert!(object.contains_key("st"));
    }

    #[test]
    fn derive_database_name_respects_override_with_directories() {
        let ingest = ingest_response_with_paths("/workspace", "/workspace/indexes/custom.sqlite");

        let result = derive_database_name_for_status(&ingest, Some("indexes/custom.sqlite".into()));

        assert_eq!(result.as_deref(), Some("indexes/custom.sqlite"));
    }

    #[test]
    fn derive_database_name_uses_relative_path_when_override_missing() {
        let ingest = ingest_response_with_paths("/workspace", "/workspace/indexes/custom.sqlite");

        let result = derive_database_name_for_status(&ingest, None);

        assert_eq!(result.as_deref(), Some("indexes/custom.sqlite"));
    }

    #[test]
    fn derive_database_name_falls_back_to_file_name_outside_root() {
        let ingest = ingest_response_with_paths("/workspace", "/tmp/index.sqlite");

        let result = derive_database_name_for_status(&ingest, None);

        assert_eq!(result.as_deref(), Some("index.sqlite"));
    }

    #[test]
    fn summarize_bundle_surfaces_primary_snippets_and_links() {
        let bundle = sample_bundle_response();
        let summary = summarize_bundle(&bundle);

        assert!(summary.contains(
            "Context bundle prepared for src/lib.rs with 1 definition(s) and 1 snippet(s)."
        ));
        assert!(summary.contains("Primary definition function foo."));
        assert!(summary.contains("Details: visibility pub."));
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
                source: SearchSource::Embedding,
                summary: None,
                symbol: None,
                identifier: None,
                source_type: None,
                metadata: None,
                confidence: 0.87,
            }],
            summary_mode: SummaryMode::Brief,
            query_intent: Some(QueryIntent::Embedding),
            intent_confidence: Some(0.75),
            context_goals: vec![ContextGoal::GeneralUnderstanding],
            clarification_prompts: Vec::new(),
            suggested_tools: Vec::new(),
            diagnostics: None,
        };

        let summary = crate::search::summarize_semantic_search(&response);

        assert!(summary.contains(
            "Semantic search scanned 250 chunk(s) and returned 1 match(es) (model custom-model)."
        ));
        assert!(summary.contains("Top hit: src/main.rs#L42 (confidence 0.87)."));
    }

    #[test]
    fn summarize_semantic_search_reports_lexical_hits_and_confidence() {
        let response = SemanticSearchResponse {
            database_path: "db.sqlite".into(),
            database_name: Some("db.sqlite".into()),
            embedding_model: Some("custom-model".into()),
            total_chunks: 200,
            evaluated_chunks: 150,
            results: vec![SemanticSearchMatch {
                path: "src/main.rs".into(),
                chunk_index: 0,
                score: 1.0,
                normalized_score: 0.92,
                language: Some("Rust".into()),
                classification: Classification::Function,
                content: "fn example() {}".into(),
                embedding_model: "custom-model".into(),
                byte_start: Some(10),
                byte_end: Some(20),
                line_start: Some(44),
                line_end: Some(47),
                context_before: None,
                context_after: None,
                source: SearchSource::Lexical,
                summary: None,
                symbol: None,
                identifier: None,
                source_type: None,
                metadata: None,
                confidence: 0.92,
            }],
            summary_mode: SummaryMode::Brief,
            query_intent: Some(QueryIntent::Lexical),
            intent_confidence: Some(0.85),
            context_goals: vec![ContextGoal::GeneralUnderstanding],
            clarification_prompts: Vec::new(),
            suggested_tools: Vec::new(),
            diagnostics: Some(SearchDiagnostics {
                total_latency_ms: 25,
                lexical_latency_ms: Some(5),
                ..Default::default()
            }),
        };

        let summary = crate::search::summarize_semantic_search(&response);

        assert!(summary.contains(
            "Semantic search scanned 150 chunk(s) and returned 1 match(es) (model custom-model)."
        ));
        assert!(summary.contains("1 lexical match(es) promoted ahead of semantic ranks."));
        assert!(summary.contains("Top hit: src/main.rs#L44 (confidence 0.92)."));
    }

    #[test]
    fn summarize_semantic_search_includes_clarification_prompts() {
        let mut response = sample_semantic_response();
        response.clarification_prompts = vec!["Specify the target module or framework.".into()];

        let summary = crate::search::summarize_semantic_search(&response);

        assert!(summary.contains("Clarify: Specify the target module or framework."));
    }

    #[test]
    fn build_search_meta_captures_clarifications_and_goals() {
        let env = EnvironmentState::new();
        let mut response = sample_semantic_response();
        response.context_goals = vec![ContextGoal::Debugging];
        response.clarification_prompts = vec!["Provide the stack trace".into()];

        let meta = env.build_search_meta(&response, 2, None);
        let info = meta
            .get("semanticSearch")
            .and_then(|value| value.as_object())
            .expect("semanticSearch meta");

        let clarifications = info
            .get("clarifications")
            .and_then(|value| value.as_array())
            .expect("clarifications array");
        assert_eq!(clarifications.len(), 1);
        assert_eq!(clarifications[0], json!("Provide the stack trace"));

        let goals = info
            .get("contextGoals")
            .and_then(|value| value.as_array())
            .expect("context goals array");
        assert_eq!(goals, &vec![json!(ContextGoal::Debugging)]);
    }

    #[test]
    fn semantic_search_structured_content_is_compact() {
        let response = sample_semantic_response();
        let meta = Meta::new();
        let result = build_semantic_search_result(response, meta, None, Vec::new(), Vec::new())
            .expect("result");
        let structured = result.structured_content.expect("structured content");
        let object = structured.as_object().expect("object");
        assert_eq!(object.get("t"), Some(&json!("sem")));
        assert!(object.contains_key("r"));
        assert!(!object.contains_key("results"));
    }

    #[test]
    fn semantic_search_result_includes_attachments_and_warnings() {
        let response = sample_semantic_response();
        let mut meta = Meta::new();
        meta.insert(
            "attachments".to_string(),
            json!({ "bundle": { "meta": true } }),
        );

        let mut attachments_map = serde_json::Map::new();
        attachments_map.insert("bundle".to_string(), json!({ "t": "ctx" }));

        let warnings = vec!["bundle truncated".to_string()];
        let summary_line = "bundle summary".to_string();

        let result = build_semantic_search_result(
            response,
            meta,
            Some(Value::Object(attachments_map)),
            warnings,
            vec![summary_line.clone()],
        )
        .expect("result");

        let summary_text = result
            .content
            .first()
            .and_then(|entry| entry.raw.as_text())
            .map(|text| text.text.clone())
            .expect("text summary");
        assert!(summary_text.contains(&summary_line));
        assert!(summary_text.contains("bundle truncated"));

        let structured = result
            .structured_content
            .expect("structured content available");
        let object = structured.as_object().expect("object payload");
        let attachments = object
            .get("att")
            .and_then(|value| value.as_object())
            .expect("attachments object");
        assert!(attachments.contains_key("bundle"));

        let warn = object
            .get("warn")
            .and_then(|value| value.as_array())
            .expect("warnings array");
        assert_eq!(
            warn.first().and_then(|value| value.as_str()),
            Some("bundle truncated")
        );

        let meta_map = result.meta.expect("meta present");
        let attachments_meta = meta_map
            .get("attachments")
            .and_then(|value| value.as_object())
            .expect("attachments meta object");
        assert!(attachments_meta.contains_key("bundle"));
    }

    #[test]
    fn attachments_meta_omitted_when_empty() {
        let acc = SearchAttachmentAccumulator::default();
        assert!(acc.attachments_meta().is_none());
    }

    #[test]
    fn attachments_meta_serializes_successful_entries() {
        let mut acc = SearchAttachmentAccumulator::default();
        let mut bundle_meta = Meta::new();
        bundle_meta.insert("detail".into(), json!("value"));
        acc.bundle(AttachmentResult::Success {
            structured: Value::Null,
            meta: Some(bundle_meta),
            summary: "bundle".into(),
        });

        let mut lookup_meta = Meta::new();
        lookup_meta.insert("lookup".into(), json!(1));
        acc.lookup(AttachmentResult::Success {
            structured: Value::Null,
            meta: Some(lookup_meta),
            summary: "lookup".into(),
        });

        let attachments = acc.attachments_meta().expect("meta present");
        let object = attachments.as_object().expect("meta object");
        assert_eq!(
            object
                .get("bundle")
                .and_then(|value| value.get("detail"))
                .and_then(|value| value.as_str()),
            Some("value")
        );
        assert_eq!(
            object
                .get("code")
                .and_then(|value| value.get("lookup"))
                .and_then(|value| value.as_i64()),
            Some(1)
        );
    }

    #[test]
    fn shared_budget_hint_respects_limits() {
        let snapshot = EnvironmentSnapshot {
            bundle_budget_override: Some(1_800),
            remaining_context_tokens: Some(500),
            ..Default::default()
        };
        let shared = SharedBudget {
            total_tokens: Some(450),
            bundle_tokens: Some(800),
            lookup_tokens: None,
        };
        assert_eq!(
            shared.bundle_budget_hint(&snapshot),
            Some(MIN_BUNDLE_BUDGET as u32)
        );
    }

    #[test]
    fn shared_budget_hint_prefers_bundle_token_cap() {
        let snapshot = EnvironmentSnapshot {
            bundle_budget_override: Some(2_000),
            remaining_context_tokens: None,
            ..Default::default()
        };
        let shared = SharedBudget {
            total_tokens: None,
            bundle_tokens: Some(750),
            lookup_tokens: None,
        };
        assert_eq!(shared.bundle_budget_hint(&snapshot), Some(750));
    }

    #[test]
    fn derive_bundle_params_require_search_results() {
        let plan = OrchestrationPlan {
            search: SemanticSearchRequest {
                root: Some("/workspace".into()),
                query: "alpha".into(),
                database_name: Some("db.sqlite".into()),
                limit: None,
                model: None,
                language: None,
                path_prefix: None,
                path_contains: None,
                classification: None,
                summary_mode: None,
                max_context_before: None,
                max_context_after: None,
            },
            include_bundle: true,
            include_lookup: false,
            bundle_override: None,
            lookup_override: None,
            shared_budget: SharedBudget::default(),
        };

        let mut response = sample_semantic_response();
        response.results.clear();

        let err = derive_bundle_params_from_search(&plan, &response).unwrap_err();
        assert!(err.contains("no results"));
    }

    #[test]
    fn resolve_lookup_mode_defaults() {
        let base = CodeLookupParams {
            root: Some("/workspace".into()),
            database_name: Some("db.sqlite".into()),
            mode: None,
            query: None,
            file: None,
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

        assert_eq!(resolve_lookup_mode(&base), "search");

        let mut file_mode = base.clone();
        file_mode.file = Some("src/lib.rs".into());
        assert_eq!(resolve_lookup_mode(&file_mode), "bundle");

        let mut explicit = base.clone();
        explicit.mode = Some("search".into());
        explicit.query = Some("alpha".into());
        assert_eq!(resolve_lookup_mode(&explicit), "search");
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
                source: SearchSource::Embedding,
                summary: None,
                symbol: None,
                identifier: None,
                source_type: None,
                metadata: None,
                confidence: 0.82,
            }],
            summary_mode: SummaryMode::Brief,
            query_intent: Some(QueryIntent::Embedding),
            intent_confidence: Some(0.78),
            context_goals: vec![ContextGoal::GeneralUnderstanding],
            clarification_prompts: Vec::new(),
            suggested_tools: Vec::new(),
            diagnostics: None,
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

    #[test]
    fn context_bundle_structured_content_is_compact() {
        let bundle = sample_bundle_response();
        let result = build_context_bundle_result(bundle, None).expect("context bundle result");
        let structured = result.structured_content.expect("structured content");
        let object = structured.as_object().expect("object");
        assert_eq!(object.get("t"), Some(&json!("ctx")));
        assert!(object.contains_key("defs"));
        assert!(!object.contains_key("definitions"));
    }
}
