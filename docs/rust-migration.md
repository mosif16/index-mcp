# Rust Server Migration Status

This workspace now contains an experimental Rust implementation of the MCP server at
`crates/index-mcp-server`. The goal is to replace the Node/TypeScript entrypoint in `src/server.ts`
with a native binary that reuses the existing SQLite index and native ingestion logic.

## Current State

- Cargo workspace scaffolded at the repository root with `index-mcp-server` alongside the existing
  `index_mcp_native` crate.
- Rust server boots over stdio using the [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)
  SDK (git dependency on `main`).
- Native tooling matches the Node implementation for first-party features:
  - `ingest_codebase` with filesystem scanning, SQLite schema upkeep, ingestion history tracking,
    chunk embeddings, TypeScript graph extraction, changed-path ingestion, and auto-eviction (80 %
    target, least-used chunks/nodes, runtime stats).
  - `semantic_search`, `context_bundle`, and `code_lookup` (search + bundle modes) with matching
    response envelopes.
  - `graph_neighbors` (GraphRAG exploration) and `repository_timeline` (git history summaries)
    implemented natively.
  - `index_status` mirrors the Node freshness checks (database metrics, embedding models, commit
    comparison).
  - `indexing_guidance_tool` + `indexing_guidance` prompt registered through the rmcp prompt API.
  - Watch mode (`--watch`, `--watch-debounce`, `--watch-no-initial`, `--watch-quiet`) drives
    incremental ingests via the Rust pipeline.
- Shared schema/database updates (hits columns, graph tables, meta entries) are respected by the
  Rust server, so `.mcp-index.sqlite` can be reused interchangeably between implementations.
- Remote MCP proxy routing (`INDEX_MCP_REMOTE_SERVERS`) now works end-to-end via the new
  `remote_proxy` module. It bootstraps remote tool descriptors, establishes SSE connections over
  `reqwest`, forwards tool calls, and recovers cleanly on transport errors.

## Progress Snapshot

- Estimated migration completion: **100 %**.
  - Core tools, GraphRAG endpoints, git timeline summaries, prompts, watcher/CLI flows, and remote
    MCP proxying all run natively in the Rust binary.

## Next Up

- Harden the remote MCP proxy with richer progress forwarding and telemetry once production usage
  highlights additional needs. Track future Node enhancements in lockstep so both servers remain
  feature complete.
- Plan a dedicated debugging and test pass across every Rust tool (ingest, search, bundles, graph,
  timeline, remotes, prompts) to verify parity under real workloads before retiring the Node
  runtime. Testing should cover:
  - **Happy-path ingest runs** on small, medium, and large repositories (graph on/off, embeddings
    on/off, auto-evict thresholds, explicit `paths`, `.gitignore` coverage) with database diffs
    validated via `index_status`.
  - **Tool surface validation** (`code_lookup`, `semantic_search`, `context_bundle`,
    `graph_neighbors`, `repository_timeline`, `index_status`, `ingest_codebase`, prompts) using the
    Rust binary only, capturing structured responses and ensuring they match the Node outputs.
  - **Remote proxy exercises** against at least one SSE-capable MCP server to confirm tool
    discovery, call routing, retry behaviour, and reconnection logic.
  - **Watcher regression tests**, including debounce tuning, changed-path ingestion, and graceful
    shutdown (`INDEX_MCP_ARGS="--watch"` via `start.sh`).
  - **Error-path drills** (invalid roots, SQLite locks, embedding failures, oversized files) with
    explicit assertions that Rust surfaces diagnostically rich `McpError` payloads.
  Document the full procedure and capture representative logs before the Node runtime is retired.

## Integration Notes

- The Rust server returns structured MCP data via `CallToolResult` with both text and JSON payloads
  to maintain backwards compatibility with existing clients.
- Ingestion writes file metadata, stored content (when enabled), embeddings, and TypeScript graph
  nodes/edges; callers can use the same SQLite database with either server.
- Auto-eviction mirrors the Node heuristics (80% target, least-used chunks/nodes, hits-based order)
  and includes eviction stats in `ingest_codebase` responses.
- Watcher ingests reuse the changed-path fast path, so long-running agents can keep the database
  fresh without leaving Node in the loop.
- Git interactions (`git rev-parse`, `git log`) run inside blocking tasks to avoid stalling the async runtime.
- Keep both implementations building in CI so schema or protocol changes are detected early.

## Usage

```bash
cargo run -p index-mcp-server          # Launch Rust server over stdio
cargo check -p index-mcp-server        # Compile and lint without running
```

Point MCP clients at the compiled binary exactly like the Node server. The instructions banner is
currently minimal and will expand as more tools land.
