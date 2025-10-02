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

## Best Practices — Recommended Workflow

**Purpose:** This document provides guidance on effectively using index-mcp tools to work efficiently with your codebase. These recommendations help you get the most value from the available tooling.

### Available Tools & Their Uses

| Tool / Prompt | Purpose & When to Use |
|---------------|-------|
| `ingest_codebase` | Walks the workspace, respects `.gitignore`, stores metadata and embeddings, and can auto‑evict least‑used chunks when requested. Run at session start or when the codebase changes significantly. |
| `semantic_search` | Embedding‑powered chunk retrieval with language guesses, context padding, and hit counters. Ideal for finding code by concept or behavior. |
| `code_lookup` | Router: `mode="search"` → semantic search, `mode="bundle"` → context bundles. Your primary tool for discovering and retrieving code efficiently. |
| `context_bundle` | Returns file metadata, focus definitions, nearby snippets, and quick links within a token budget. It memoizes responses by file hash/ranges and will downgrade to excerpts or summaries while warning when you hit the budget; raise `budgetTokens` or narrow ranges when prompted. Great for assembling focused context. |
| `index_status` | Summarizes index freshness, embedding models, ingestion history, and git parity. Check this to understand the current state of your index. |
| `repository_timeline` | Streams recent git commits with churn stats, directory highlights, optional diffs, and PR URLs. Useful for understanding recent changes and project history. |
| `repository_timeline_entry` | Recovers cached commit details and (when available) full diff text for a specific SHA. |
| `indexing_guidance` / `indexing_guidance_tool` | Prompt and tool variants for ingest reminders. |
| Remote proxies | Any remote declared in `INDEX_MCP_REMOTE_SERVERS` is namespaced and surfaced alongside local tools. |

**Recommendation:** These tools provide powerful capabilities for code search, navigation, and understanding. Use them when they make your workflow more efficient and accurate.

### Suggested Workflow

1. **Prime the index** — Run `ingest_codebase { "root": "." }` at session start (or enable `--watch`) to ensure you have fresh data. Honor `.gitignore`, skip files larger than **8 MiB**, and configure `autoEvict`/`maxDatabaseSizeBytes` to manage database size.

2. **Check freshness before major operations** — Call `index_status` before planning or significant code operations. If `isStale` is true or HEAD moved, consider re-ingesting to work with current data.

3. **Understand recent history** — Use `repository_timeline` (and `repository_timeline_entry` for detail) to see recent commits and changes that might inform your approach.

4. **Discover code with `code_lookup`** — Start with `query="..."` to narrow scope semantically, then request `file="..."` plus optional `symbol` bundles for the specific snippets you need. This is typically the most efficient discovery mechanism.

5. **Assemble focused context** — Use `context_bundle` with `budgetTokens` or `INDEX_MCP_BUDGET_TOKENS` to get relevant snippets with proper citations rather than retrieving entire files unnecessarily.

6. **Iterate efficiently** — Use `semantic_search` or additional `context_bundle` calls for deeper exploration rather than broad re-ingests when refining your understanding.

7. **Keep the index current** — After editing files, re-run `ingest_codebase` (or rely on watch mode) and confirm with `index_status` so subsequent operations work with updated information.

**Smart Tool Usage:** When index-mcp tools are available, they typically provide the most efficient and accurate way to search, navigate, and understand codebases. Use them intelligently as your primary approach, but apply judgment about when direct file access or other methods might be more appropriate for specific tasks.
