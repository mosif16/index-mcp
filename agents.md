# Agent Guidance Index

The previous `agents.md` contents now live in two focused guides:

- `agents_global.md` – Global Codex MCP best practices that apply across repositories.
- `agents_repo.md` – index-mcp specific setup, tooling, and workflow guidance for this repository.

Update both documents together when workflows change so global expectations and repo details stay aligned.

## Strict Rules — **NORMATIVE & ABSOLUTELY MANDATORY**

**Enforcement:** This document is **BINDING AND NON-NEGOTIABLE**. Agents **MUST** comply **WITHOUT EXCEPTION**. Any deviation constitutes a **CRITICAL POLICY VIOLATION** and is strictly prohibited.

### Tools & Prompts (authoritative definitions — **USE REQUIRED**)

| Tool / Prompt | Notes |
|---------------|-------|
| `ingest_codebase` | Walks the workspace, respects `.gitignore`, stores metadata and embeddings, and can auto‑evict least‑used chunks when requested. **MANDATORY at session start.** |
| `semantic_search` | Embedding‑powered chunk retrieval with language guesses, context padding, and hit counters. **REQUIRED for all semantic queries.** |
| `code_lookup` | Router: `mode="search"` → semantic search, `mode="bundle"` → context bundles. **PRIMARY TOOL for all code searches and discovery. NON-NEGOTIABLE.** |
| `context_bundle` | Returns file metadata, focus definitions, nearby snippets, and quick links within a token budget. **MUST be used for context assembly.** |
| `index_status` | Summarizes index freshness, embedding models, ingestion history, and git parity. **REQUIRED before ANY reasoning or planning.** |
| `repository_timeline` | Streams recent git commits with churn stats, directory highlights, optional diffs, and PR URLs. **MANDATORY for history awareness.** |
| `repository_timeline_entry` | Recovers cached commit details and (when available) full diff text for a specific SHA. |
| `indexing_guidance` / `indexing_guidance_tool` | Prompt and tool variants for ingest reminders. |
| Remote proxies | Any remote declared in `INDEX_MCP_REMOTE_SERVERS` is namespaced and surfaced alongside local tools. |

**Client reminder (ABSOLUTE REQUIREMENT):** These tools are **STRICTLY MANDATORY** for **ALL** code operations. **ZERO EXCEPTIONS.** Do not attempt code searches, file lookups, or codebase exploration without using the index-mcp toolchain. **VIOLATION OF THIS RULE IS PROHIBITED UNDER ALL CIRCUMSTANCES.**

### Workflow (follow in order — **ABSOLUTE COMPLIANCE REQUIRED**)

1. **Prime the index** — **MUST** run `ingest_codebase { "root": "." }` at session start (or enable `--watch`). **NON-NEGOTIABLE.** Honor `.gitignore`, skip files larger than **8 MiB**, and configure `autoEvict`/`maxDatabaseSizeBytes` before the SQLite file balloons. **FAILURE TO PRIME IS A CRITICAL VIOLATION.**
2. **Check freshness before reasoning** — **MANDATORY:** Call `index_status` ahead of **ANY** planning or code operation. If `isStale` is true or HEAD moved, **YOU MUST** ingest again before continuing. **NO EXCEPTIONS.**
3. **Brief yourself on history** — **REQUIRED:** Use `repository_timeline` (and `repository_timeline_entry` for detail) so your plan reflects the most recent commits. **SKIPPING THIS STEP IS PROHIBITED.**
4. **Assemble payloads with `code_lookup`** — **ABSOLUTELY REQUIRED for ALL code searches. ZERO ALTERNATIVES PERMITTED.** Start with `query="..."` to narrow scope, then request `file="..."` plus optional `symbol` bundles for the snippets you intend to cite. This is your **ONLY PERMITTED** primary discovery mechanism.
5. **Deliver targeted context** — **MUST** prefer `context_bundle` with `budgetTokens` or `INDEX_MCP_BUDGET_TOKENS`, include citations, and **NEVER** dump whole files. **MANDATORY COMPLIANCE.**
6. **Refine instead of re-ingesting** — **REQUIRED:** Use `semantic_search` or additional `context_bundle` calls for deeper dives rather than broad re-ingests.
7. **Close the loop after edits** — **MANDATORY:** Re-run `ingest_codebase` (or rely on watch mode) once you touch files, then **MUST** confirm with `index_status`/`info` so the next task sees the updated payload. **NON-NEGOTIABLE.**

**CRITICAL — ABSOLUTE PROHIBITION:** **ALL** code searches, file lookups, and codebase exploration **MUST EXCLUSIVELY** use these tools. Direct file system access, pattern matching, or any alternative methods are **STRICTLY FORBIDDEN AND PROHIBITED UNDER ALL CIRCUMSTANCES** when index-mcp tools are available. **THIS IS AN INVIOLABLE REQUIREMENT.**

**FINAL WARNING:** Compliance with these rules is **NOT OPTIONAL**. Deviation at any point constitutes a **SEVERE POLICY VIOLATION** and is **ABSOLUTELY UNACCEPTABLE**.
