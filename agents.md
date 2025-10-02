# Agent Guidance Index

The previous `agents.md` contents now live in two focused guides:

- `agents_global.md` – Global Codex MCP best practices that apply across repositories.
- `agents_repo.md` – index-mcp specific setup, tooling, and workflow guidance for this repository.

Update both documents together when workflows change so global expectations and repo details stay aligned.

## 5. Typical Workflow

1. **Prime the index** – Run `ingest_codebase { "root": "." }` at session start (or enable `--watch`). Honor `.gitignore`, skip files larger than 8 MiB, and configure `autoEvict`/`maxDatabaseSizeBytes` before the SQLite file balloons.
2. **Check freshness before reasoning** – Call `index_status` ahead of planning. If `isStale` is true or HEAD moved, ingest again before continuing.
3. **Brief yourself on history** – Use `repository_timeline` (and `repository_timeline_entry` for detail) so your plan reflects the most recent commits.
4. **Assemble payloads with `code_lookup`** – Start with `query="..."` to narrow scope, then request `file="..."` plus optional `symbol` bundles for the snippets you intend to cite.
5. **Deliver targeted context** – Prefer `context_bundle` with `budgetTokens` or `INDEX_MCP_BUDGET_TOKENS`, include citations, and avoid dumping whole files.
6. **Refine instead of re-ingesting** – Use `semantic_search` or additional `context_bundle` calls for deeper dives rather than broad re-ingests.
7. **Close the loop after edits** – Re-run `ingest_codebase` (or rely on watch mode) once you touch files, then confirm with `index_status`/`info` so the next task sees the updated payload.
