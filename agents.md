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
## Required Local Testing

With the GitHub workflows removed, agents must run these checks before handing work back to the user:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-targets`

If any command cannot be executed, explain why in the final response and highlight follow-up steps for the user.

## Agent Policy — Tool Chaining for Efficient, Context‑Aware Workflows

### Detailed Tool Chaining Playbook
**Purpose**
- Translate high-level policy into an actionable, auditable workflow that every agent can follow without improvisation.
- Surface the minimum telemetry to prove compliance (timestamps, tool order, token budgets).
- Define recovery paths when a link in the chain fails (missing index, stale cache, sandbox denials).

**Preflight Checklist (run once per task)**
1. Confirm `cwd` points to the active workspace root; export `INDEX_MCP_ROOT` if your client supports it.  
2. Set or verify `INDEX_MCP_BUDGET_TOKENS` (default 3_500).  
3. Ensure no outstanding edits need ingestion; if unsure, re-run `ingest_codebase` before touching anything else.  
4. Capture the ticket goal in your scratchpad so you can reference it in tool call notes.  
5. If the task touches Supabase, pause and obtain explicit user approval before continuing.

**Canonical Flow With Guardrails**
| Step | Tool | Mandatory Inputs | Expected Output | Failure Recovery |
| --- | --- | --- | --- | --- |
| 1 | `index_status` | `root`, `databaseName` | Freshness + commit SHA | If `isStale` or SHA mismatch → run Step 2 immediately. |
| 2 | `ingest_codebase` | Same root; optional include/exclude | Ingestion summary, chunk counts | On failure → inspect stderr, resolve permissions, retry; do not advance until it succeeds. |
| 3 | `index_status` (confirm) | Same as Step 1 | `isStale=false` confirmation | If still stale → re-ingest with narrower include list or check for git HEAD changes. |
| 4 | `semantic_search` | Focused query text | Ranked hits + tool suggestions | If zero hits → tighten keywords, add file hints, or broaden query. |
| 5 | Follow suggestion | Typically `context_bundle` or `code_lookup` | Focused snippets respecting budget | If bundle empty → verify file indexed, rerun with ranges/focus line. |
| 6 | Optional refinements | Repeat Steps 4-5 with narrower scope | Additional evidence | Stop once you have cited material; avoid redundant bundles. |
| 7 | Action phase | `shell` edits/tests or MCP write tool | Code changes + validation logs | After edits → re-run Step 2 to refresh the index. |
| 8 | Post-change `index_status` | Same as Step 1 | Green status | If stale → ingestion drift likely; rerun Step 2. |
| 9 | Final Ops Log | N/A | Ordered list of tool calls + context usage | Required in every final reply. |

**Decision Triggers**
- Need repo history? Call `repository_timeline` **only after** Step 3, and scope `limit` ≤ 5.
- Need symbol-specific context? Use `code_lookup` with `mode="bundle"` and `symbol` populated; avoid manual globbing.
- Exhausted local context? Document what is missing, then run `tavily-search` with explicit citations in your response.
- Encounter sandbox denial? Retry the same command with `with_escalated_permissions=true` and a one-sentence justification, unless policy is `never` (default here).

**Telemetry You Must Track**
- Timestamp each tool call in your scratchpad (HH:MM local is sufficient).  
- Record token budgets requested vs. consumed (see `context_bundle` response `usage`).  
- Capture any warnings returned by tools; echo them in the final Operation Log so the user can act.

**Failure Recovery Patterns**
- `semantic_search` returns outdated paths → rerun Steps 2-3, then repeat the query.  
- `context_bundle` omits expected symbols → pass `focusLine` or `ranges`, or confirm the symbol via `code_lookup mode="search"`.  
- Tool request rejected (Supabase/no approval) → stop and request direction; never attempt alternative credentials.  
- Embedder missing → note the failure, fall back to lexical search (`code_lookup mode="search"`), and call it out in the Operation Log.

**Example Session (abbreviated)**
1. `index_status {"root":".","databaseName":"default"}` → stale.  
2. `ingest_codebase {"root":".","databaseName":"default"}` → 643 chunks ingested.  
3. `index_status {"root":".","databaseName":"default"}` → fresh.  
4. `semantic_search {"query":"ingest pipeline error","limit":5}` → suggestions include a `context_bundle` for `src/ingest.rs`.  
5. `context_bundle {"file":"crates/index-mcp-server/src/ingest.rs","focusLine":520,"budgetTokens":2000}` → snippet confirms suspected regression.  
6. `shell` command (`cargo test --all --all-targets`) → reproduces failure locally.  
7. Apply fix, then rerun `ingest_codebase` and `index_status` to refresh the index.  
8. Compile Operation Log summarizing steps, budgets, warnings, and validation status.

**Final Message Requirements**
- Cite every file/line referenced using repository-relative paths.  
- Include the ordered tool list, token budgets, and key evidence in an Operation Log section.  
- If any mandated step could not run, say why and suggest follow-ups.
