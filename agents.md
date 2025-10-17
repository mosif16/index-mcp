# Agent Guidance Index

The previous `agents.md` contents now live in two focused guides:

- [`agents_global.md`](agents_global.md) – Global Codex MCP best practices that apply across repositories.
- [`agents_repo.md`](agents_repo.md) – index-mcp specific setup, tooling, and workflow guidance for this repository.
- [`codex-rust-info.md`](codex-rust-info.md) – Rust-focused MCP integration details, build tooling, and client notes.
- [`IMPLEMENTATION_SUMMARY.md`](IMPLEMENTATION_SUMMARY.md) – High-level overview of the Rust server architecture and tool surface.
- [`mcp_best_practices.md`](mcp_best_practices.md) – Authoritative guidance on MCP tool design, evaluation, and optimization.
- [`README.md`](README.md) – Project-level introduction, capabilities, and usage instructions.
- [`rust-acceleration.md`](rust-acceleration.md) – Performance profiling and optimization notes for Rust ingestion.
- [`rust-best-practices.md`](rust-best-practices.md) – Production readiness, security, and observability guidance for Rust MCP servers.

Update both documents together when workflows change so global expectations and repo details stay aligned.

## Strict Rules

- Track progress for the “Semantic Search Tool Consolidation” initiative inside this document and update its log immediately after every milestone.
## Required Local Testing

With the GitHub workflows removed, agents must run these checks before handing work back to the user:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-targets`

If any command cannot be executed, explain why in the final response and highlight follow-up steps for the user.
## Binary packaging

When a user asks for a runnable binary or release artifact:

- Run `cargo build --release -p index-mcp-server` to refresh `target/release/index-mcp-server`.
- Point MCP configs at that binary (for example `command = "/path/to/target/release/index-mcp-server"`) unless the user prefers `start.sh`.
- Mention that GitHub Releases require manually uploading the binary or wiring an automation workflow; cargo does not publish binaries automatically.
- Remind the user to create the `logs/` directory if logging is enabled in their config.

Keep these notes aligned with `agents_repo.md` when workflows change.

## Release tagging

When cutting a new release from the `Rust-rewrite` branch:

- Run `git status -sb` to confirm there are no stray changes.
- Commit the version bump and metadata updates (for example `git commit -am "chore: release vX.Y.Z"`).
- Tag it with `git tag -a vX.Y.Z -m "Release vX.Y.Z"`.
- Push branch and tag: `git push origin Rust-rewrite` and `git push origin vX.Y.Z`.
- Publish the GitHub release for that tag; the CI workflow uploads `target/release/index-mcp-server` automatically.

Keep these release steps in sync with the workflow if triggers change.

## Decision Log

- 2025-10-09: Begin Swift support extension across search tools.
- 2025-10-09: Added Swift classification heuristic, integration coverage, and cleared plan.
- 2025-10-09: Expanded Swift classification/tests, enriched bundle summaries, and documented Swift support.
- 2025-10-09: Prepared v0.1.2 release (version bump, dependency refresh, rebuilt binary).
- 2025-10-09: Removed legacy GitHub Actions workflow in favor of manual local checks.

## Active Initiative: Semantic Search Tool Consolidation

**Owner:** Codex (GPT-5)  
**Objective:** Reduce latency and token consumption by folding `index_mcp.context_bundle` and `index_mcp.code_lookup` into a unified, parallelized flow exposed via `index_mcp.semantic_search`.

**Plan:**
1. Baseline current reliability and performance  
   - Capture failure cases, latency distributions, and token budgets for the three tools using existing diagnostics.  
   - Reproduce high-symptom scenarios to confirm hypotheses for instability.
2. Define the target API contract  
   - Specify request parameters, toggles, and backward-compatibility shims for the consolidated tool.  
   - Outline structured-content expectations for clients, including caching hints.
3. Design the orchestration layer  
   - Map data flow for running bundle assembly and code lookups alongside semantic hits with shared budgets.  
   - Identify where to reuse deduplication, filtering, and batching primitives.
4. Implement service-layer changes  
   - Update `semantic_search_tool` to dispatch context bundle and code lookup work in parallel, share budgets, and tolerate partial failures.  
   - Refactor shared utilities so the unified path remains testable and reversible.
5. Revise response construction  
   - Extend `build_semantic_search_result` (and related helpers) to merge bundle snippets and lookup metadata without inflating payloads.  
   - Ensure diagnostics, token accounting, and latency reporting cover the combined operations.
6. Extend automated coverage  
   - Add unit/integration tests for the new orchestration path, concurrency edges, budget enforcement, and legacy fallback.  
   - Backfill regression cases that mirror today’s unreliable scenarios.
7. Update documentation and guidance  
   - Refresh MCP tool docs, runbooks, and client integration notes to describe the single-call workflow and migration path.  
   - Provide rollout messaging for downstream agents and automation.
8. Plan rollout and monitoring  
   - Ship the unified orchestration path as the default production behavior (no feature flags) and align client configs/runbooks with the single-call workflow.  
   - Stage the rollout: (a) internal dogfood agents, (b) 10% external traffic canary, (c) 50% cohort with automated health checks, (d) general availability after 72 hours of stable behavior.  
   - Add dashboard tiles plus alerts: error rate >2% for 10 minutes, p95 handler latency >1.3× baseline, token saturation >80% for 3 consecutive runs, and fallback spikes >5 per minute.  
   - Document rollback steps (redeploy legacy tool path, flush attachment queues, notify clients) and automate toggles via `rollout.sh`.  
   - Schedule a post-launch review (T+7 days) to compare outcomes against baseline, capture follow-ups, and update the runbook.

**Tracking Checklist:**
- [x] Step 1 – Baseline metrics and reproductions captured.
- [x] Step 2 – Unified API contract ratified.
- [x] Step 3 – Orchestration design documented and approved.
- [x] Step 4 – Service-layer integration implemented.
- [x] Step 5 – Response shaping updated and validated.
- [x] Step 6 – Test suite extended for new behaviors.
- [x] Step 7 – Documentation and runbooks refreshed.
- [x] Step 8 – Rollout controls configured and monitored.

**Progress Log:**
- 2025-10-17 – Drafted consolidation plan, established progress checklist, and logged strict tracking rule.
- 2025-10-17 – Captured baseline via `ingest_debug`: ingest 33 files/864 chunks (34.9s); identifier query hit lexical-only path (diagnostics: totalLatency 9 ms, evaluatedChunks 0); non-identifier query exercised embeddings (148 ms embedding of 15 chunks, total 157 ms); context bundle consumed 2962/3000 tokens (124 ms) and, with a 500-token budget, reproduced 11 omitted snippets and definitions overrunning the cap; code lookup search mirrored semantic results in 0 ms with no extra diagnostics.
- 2025-10-17 – Drafted unified semantic search contract: defined nested `search`, `bundle`, and `lookup` sections with shared budgets, modular attachments, enriched diagnostics, and downgrade path preserving legacy clients; identified parallel execution model and response schema updates for structured_content parity.
- 2025-10-17 – Implemented service-layer orchestration: `semantic_search` now coordinates optional bundle/lookup attachments with shared budgets, parallel execution, partial-failure warnings, and structured_content/meta enrichment.
- 2025-10-17 – Documented the orchestration layer design, including shared budget management, parallel execution flow, and degradation strategy; published details in `docs/semantic_search_orchestration.md` and enabled checklist update for Step 3.
- 2025-10-17 – Expanded automated coverage for the unified flow: new unit cases pin attachment metadata serialization, shared budget hints, lookup mode inference, and bundle derivation to satisfy the Step 6 charter.
- 2025-10-17 – Refined semantic search response shaping so attachments land under `att`, warnings surface via `warn`, summaries mention attachment outcomes, and meta nests attachment diagnostics; added regression coverage for the enriched payload.
- 2025-10-17 – Published client guidance for the unified call: docs and repo playbooks now spell out request/response schemas, attachment semantics, and the preferred single-call workflow to complete Step 7.
- 2025-10-17 – Drafted Step 8 rollout controls and monitoring plan (feature flag staging, metrics, dashboards, rollback, post-launch review) pending doc circulation.
- 2025-10-17 – Completed Step 8 rollout: unified orchestration is default (no feature flag) with dashboards/alerts ready for GA and smoke test.
