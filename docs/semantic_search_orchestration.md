# Semantic Search Orchestration Design

## Context
- `semantic_search_tool`, `context_bundle_tool`, and `code_lookup` currently execute independently, forcing clients to chain calls and re-hydrate shared metadata.
- The unified `semantic_search` path must drive lookup and bundle construction in parallel while respecting the single-call contract drafted in Step 2.
- Existing helpers (`apply_*_defaults`, `deduplicate_search_results`, bundle summarizers) already encode business rules that should remain authoritative.

## Goals
1. Reuse the core search pipeline for both plain semantic hits and the code lookup surface.
2. Share token and latency budgets across all sub-operations without double-counting chunk usage.
3. Emit consistent diagnostics, previews, and structured content regardless of which optional sections are requested.
4. Degrade gracefully: partial failures attach warnings while returning whichever sections succeeded.

## Flow Overview
1. **Request normalization**
   - Deserialize the new unified params, splitting them into `SemanticSearchRequest`, `ContextBundleParams`, and `CodeLookupParams`.
   - Apply environment defaults via the existing helper trio, then compute a `SharedBudget` (counts for total tokens, snippets, and wall-clock deadline).
2. **Planning**
   - Build an `OrchestrationPlan` struct capturing which attachments the client requested (`include.bundle`, `include.lookup`), any filters, and required metadata (ANN availability, model choice, graph neighbors).
   - Pre-compute chunk caps per section: reuse `bundle_budget()` for bundles and `estimate_token_cost()` for search previews to avoid exceeding the shared token ceiling.
3. **Execution**
   - Kick off search, lookup, and bundle tasks via `tokio::try_join!`, guarded by the shared deadline. Each task receives a `BudgetLease` that tracks tokens and lines consumed.
   - The search task reuses `perform_semantic_search` and the existing deduplication logic, returning both the raw response and the filtered match list for downstream consumers.
   - The bundle task invokes the current bundle service with the planned snippet cap and reuses the deduped match list when the client requested “search-derived snippets”.
   - The lookup task follows today’s `code_lookup` modes (`search` vs `bundle`) but sources match metadata from the shared search results when possible instead of issuing a second query.
4. **Assembly**
   - Feed the successful sections into a new `UnifiedSearchResult` builder that wraps `build_semantic_search_result`, `build_context_bundle_result`, and `build_code_lookup_result`.
   - Attach a consolidated diagnostics block recording per-section latency, token consumption, cache hits, and any warnings raised by the BudgetLeases.

## Failure & Degradation Path
- Timeouts or hard errors in one section produce a warning entry; the orchestration layer returns other completed sections and marks the failed section as `null`.
- If the primary search fails, skip bundle/lookup execution and return an error consistent with current behavior (search remains the authoritative dependency).
- When bundle construction exceeds the leased budget, truncate snippets and add a `budget_truncated` flag for downstream prompts.

## Reuse & Extensibility
- Deduplication uses the existing `deduplicate_search_results` helper and history tracking to avoid resurfacing recently served chunks.
- Batch execution reuses `perform_semantic_search`’s ANN path; the bundled neighbors list simply references the `GraphNeighbor` metadata already emitted by context bundles.
- The `OrchestrationPlan` is intentionally serializable so future steps can cache the plan or replay it for diagnostics.

## Instrumentation & Rollout Notes
- Emit span-level tracing around each task with the shared correlation ID used in Step 2’s contract, enabling dashboards to slice by section latency.
- Unified orchestration is GA by default (no feature flag); use the staged canary checklist and `rollout.sh` automation for progressive exposure and emergency rollback to the legacy tool path if needed.

## Implementation Status
- **Service orchestration shipped (2025-10-17):** `semantic_search` now accepts the unified request payload, plans bundle/lookup attachments, applies shared budget hints, executes attachments alongside search, and downgrades failures into warnings rather than aborting the tool call. Structured content gains an `att` object for bundle/code payloads plus `warn` arrays, while top-level meta nests attachment diagnostics under `attachments`.
- **Partial failures handled:** Attachment errors surface in the response warning list and do not cancel the primary search.
- **Summary + diagnostics:** The tool summary now annotates bundle/lookup completion, and attachment metadata is merged into the search meta map for downstream reporting.
- **Client docs refreshed (2025-10-17):** Request/response schema, examples, and runbook guidance are codified below so agents integrate the unified flow without reverse-engineering test cases.

## Request Schema & Examples

`semantic_search` now accepts a single JSON payload that bundles search plus optional attachments:

```jsonc
{
  "root": "/workspace",
  "query": "alpha(value: i32)",
  "databaseName": ".mcp-index.sqlite",
  "limit": 8,
  "include": {
    "bundle": true,
    "lookup": false
  },
  "bundle": {
    "file": "src/lib.rs",
    "maxSnippets": 6
  },
  "lookup": {
    "mode": "search",
    "query": "alpha"
  },
  "sharedBudget": {
    "totalTokens": 2800,
    "bundleTokens": 1200,
    "lookupTokens": 600
  }
}
```

- `search` fields correspond to legacy `SemanticSearchRequest` keys (left at the top level for compatibility).
- `include.bundle` / `include.lookup` flip specific attachments on without supplying explicit overrides. Providing `bundle` or `lookup` blocks implicitly enables the attachment even if `include.*` is `false`.
- `bundle` reuses `ContextBundleParams` – set `maxSnippets`, `maxNeighbors`, or `focusLine` when you want more context than the planner’s default. Omit the block to auto-derive parameters from the top search hit.
- `lookup` mirrors `CodeLookupParams`. When `mode` is omitted, the server chooses `search` if `query` is present, otherwise `bundle` if `file` is set.
- `sharedBudget` hints overall and per-attachment token ceilings. Budgets are advisory; the server enforces the smallest applicable cap and reports truncation through warnings and bundle usage stats.

## Response Shape

`structured_content` remains compact (for example `{"t":"sem","r":[...],"diag":{...}}`) with two additions:

- `att`: JSON object keyed by attachment (`bundle`, `code`). Each value is the compact payload already returned by the standalone tools (`{"t":"ctx",...}` or `{"t":"code",...}`).
- `warn`: Array of human-readable warnings. Attachment failures, budget truncation, or skipped operations populate this list instead of aborting the call.

The `meta` map now nests attachment diagnostics under `attachments`, preserving cache hits, bundle budgets, and any lookup filters. Empty attachment meta is omitted entirely.

The text summary concatenates:
1. The canonical semantic search summary.
2. One line per successful attachment (for example `bundle attachment produced context for src/lib.rs`).
3. A final `Warnings:` line if the `warn` array is non-empty.

## Operational Runbook

- Prefer the unified call whenever you would otherwise chain `semantic_search` → `context_bundle` / `code_lookup`. The orchestrator deduplicates search results and shares budgets automatically.
- Use `include.*` for lightweight “give me everything” defaults; pass explicit `bundle` / `lookup` blocks when you need tight control over snippet counts, symbol selection, or lookup filters.
- Monitor `warn` for degradations. Transient issues (timeouts, missing files) leave the primary search intact but should surface in dashboards.
- Budget tuning: set `sharedBudget.totalTokens` to the maximum combined payload you can tolerate, then ratchet `bundleTokens` / `lookupTokens` downward to reserve headroom for search results. The server also factors in environment-reported `remainingContextTokens`.
- Legacy clients can continue to call standalone tools; the unified path is default, and rollback simply involves redeploying the prior release if we need to restore the legacy tool chain.
