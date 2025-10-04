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

**Purpose:** Define the *mandatory* chain of MCP tools that agents must follow to stay fast, fresh, and token‑efficient while maintaining accurate context. This policy removes ambiguity about which tool runs first, which tool routes follow‑ups, and how to keep the index authoritative.

---

### Tool Roles (authoritative)

| Tool / Prompt | Role & Routing | Notes |
|---|---|---|
| `ingest_codebase` | **Index priming & updates** | Always pass **absolute** `root` (e.g., `/Users/.../repo`). Honor `.gitignore`. Skip files **> 8 MiB**. Tune `autoEvict`/`maxDatabaseSizeBytes`. Prefer `--watch` when available. |
| `index_status` | **Freshness gate** | Call before planning/answering. If `isStale` or HEAD moved, **re‑ingest** before continuing. |
| `repository_timeline` | **Recent history brief** | Summarize latest commits & churn to steer exploration. |
| `repository_timeline_entry` | **Deep dive commit** | Fetch cached details/diffs for a specific SHA when planning changes. |
| `semantic_search` | **Primary discovery tool** | **Start here for exploration.** Embedding‑powered retrieval with ranked suggestions. Use to find concepts, behaviors, or entry points before deeper context assembly. |
| `code_lookup` | **Routing & orchestration** | Router tool: `mode="search"` mirrors semantic search + suggestions; `mode="bundle"` fetches focused context. Use after initial discovery to assemble targeted context. |
| `context_bundle` | **Compact, focused context** | Assemble trimmed, cited snippets for selected files/symbols. Always set `budgetTokens` (or `INDEX_MCP_BUDGET_TOKENS`). Called directly or via `code_lookup` bundles. |
| `indexing_guidance` / `indexing_guidance_tool` | Operational reminders | Use for quick diagnostics and best‑practice prompts. |
| `info` | Diagnostics | Environment/runtime metadata and sanity checks. |

> **Design principle:** `semantic_search` discovers the space; `code_lookup` orchestrates follow‑ups; `context_bundle` assembles the final payload.

---

### Mandatory Tool Chain (do not deviate)

**Step 0 — Workspace root & budgets**  
- Always compute the **absolute** workspace root and reuse it for every call.  
- Set `INDEX_MCP_BUDGET_TOKENS` globally (start around **3000–4000** for bundles).  
- Never dump whole files; aim for minimal, cited snippets.

**Step 1 — Prime the index**  
Run `ingest_codebase {"root": "{ABSOLUTE_ROOT}"}` (or enable `--watch`). Respect `.gitignore`. Skip files > 8 MiB. Tune `autoEvict`/`maxDatabaseSizeBytes` before the SQLite file balloons.

**Step 2 — Freshness gate**  
Call `index_status`. If `isStale: true` **or** HEAD moved: run `ingest_codebase` again, then recheck `index_status`.

**Step 3 — Brief on recent changes**  
Call `repository_timeline` (optionally `repository_timeline_entry` for SHAs that matter). Use this to aim your first search at what recently changed.

**Step 4 — Discover with semantic search (start exploration here)**  
Call `semantic_search` with `query="..."` to get lightweight hit summaries and ranked follow‑up suggestions. Review the suggested tool chain before proceeding.

**Step 5 — Execute guided follow‑ups**  
Use the auto‑generated suggestions from `semantic_search` results:
- Execute suggested `context_bundle` payloads for focused context assembly, or
- Use suggested `code_lookup` calls to route into deeper exploration, or
- Follow suggested `repository_timeline` links for change context.

**Step 6 — Assemble focused bundles via router**  
When ready for detailed context, call `code_lookup` with `mode="bundle"`, `file="..."`, and optional `symbol="..."` to get compact, budgeted snippets. Pass `budgetTokens` or rely on `INDEX_MCP_BUDGET_TOKENS`.

**Step 7 — Fill gaps intelligently**  
If bundles miss details, either:  
- Refine the **semantic search** query with tighter terms, or  
- Use `code_lookup` with `mode="search"` to mirror search + suggestions, or
- Run additional narrowly scoped `semantic_search` for cross‑checking patterns.

**Step 8 — Assemble answer payload**  
Prefer **one** final `context_bundle` (direct or via `code_lookup` bundle mode) with tight `budgetTokens`. Include **file path + line range** citations. Avoid overlapping or duplicate spans.

**Step 9 — After edits**  
Once code changes are applied (outside the scope of this toolchain), immediately re‑run `ingest_codebase` (or rely on `--watch`) and confirm with `index_status` so subsequent steps see the updated code.

**Step 10 — Deliver**  
Answer concisely. Provide citations. No full‑file dumps. If more detail is requested, iterate Steps 4–8 with stricter targeting.

---

### Chain Recipes (copy‑driven playbooks)

**A) Understand a feature or bug surface**
1. `index_status` → gate  
2. `repository_timeline` → recent churn  
3. `semantic_search query="{feature|bug}"` → ranked hits + suggestions  
4. Execute suggested `context_bundle` or `code_lookup` payloads (budgeted)  
5. (Optional) Additional `semantic_search` for confirming patterns  
6. Final bundle → answer w/ citations

**B) Locate implementation & prepare minimal context**
1. `semantic_search query="{domain phrase}"` → hits + suggestions  
2. Follow suggested `context_bundle` with `file="..." symbol?` → focused snippets  
3. If incomplete: refine `semantic_search query`; use `code_lookup mode="search"` for routing  
4. Final bundle with line‑precise citations

**C) Post‑change refresh**
1. Re‑run `ingest_codebase` or rely on `--watch`  
2. `index_status` must be green  
3. Resume at **Step 4** for the next task

---

### Budget & Precision Heuristics
- Start bundles at **3–4k tokens**; raise only when strictly necessary.  
- Prefer multiple **small** bundles over one giant context.  
- De‑duplicate overlapping snippets and collapse near‑adjacent ranges.  
- Prefer files touched in the **recent timeline** when ranking equal options.  
- If a query is noisy, tighten terms or add an expected symbol/type.
- Let `semantic_search` suggestions guide the next tool hop instead of immediately fetching large bundles.

---

### Strict Prohibitions
- Do **not** bypass `semantic_search` discovery; always scout before assembling context.  
- Do **not** dump entire files.  
- Do **not** ignore token budgets or warnings.  
- Do **not** proceed with stale indexes.  
- Do **not** use external shells/interactive editors in place of MCP tools.
- Do **not** skip the suggested follow‑ups from `semantic_search` results.

---

### Compliance
All agents must follow this chain. Deviations reduce accuracy and waste tokens. When uncertain, **start with `semantic_search`**, follow its suggestions, keep context compact, and re‑check freshness.

