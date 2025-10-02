# Implementation Summary: index-mcp Rust Server

This document describes the Rust implementation that now powers the `index-mcp` Model Context Protocol server. The binary `index-mcp-server` supersedes the legacy Node/TypeScript runtime and exposes an identical tool surface through the [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) stack.

## Tool Surface & Routing

- `crates/index-mcp-server/src/service.rs` wires the standard MCP tools (`ingest_codebase`, `semantic_search`, `context_bundle`, `code_lookup`, `index_status`, `repository_timeline`, `repository_timeline_entry`, `indexing_guidance`, and `info`) and forwards requests to the underlying modules while applying consistent error handling and summary strings ([service.rs:1-115,293-341]).
- `RemoteProxyRegistry` loads JSON descriptors from the `INDEX_MCP_REMOTE_SERVERS` environment variable and mounts the advertised remote tools under a namespaced name, allowing the Rust server to proxy additional MCP services ([remote_proxy.rs:1-202]).
- Prompt instructions embedded in `service.rs` keep clients on the mandated workflow: ingest first, check freshness with `index_status`, gather history via `repository_timeline`, and rely on `code_lookup` bundles for citations.

## Ingestion Pipeline (`crates/index-mcp-server/src/ingest.rs`)

- `perform_ingest` resolves the workspace root, applies default include/exclude glob sets (skipping `.git`, build artifacts, and `.mcp-index.sqlite`), and optionally restricts ingestion to targeted paths ([ingest.rs:1-120,233-317]).
- The walker hashes files, respects a configurable max size, and stores metadata and (optionally) file contents in the `files` table. Chunks are built with `fastembed` using a cached embedder, then written to `file_chunks` with embeddings, byte/line ranges, and hit counters ([ingest.rs:57-206,703-317]).
- Each ingest writes an `ingestions` row, updates the `meta` table with the current `commit_sha` (via `git rev-parse`) and `indexed_at` timestamp, and purges removed files before committing ([ingest.rs:434-472]).
- Source files feed a lightweight TypeScript-oriented code graph extractor that populates `code_graph_nodes`/`code_graph_edges`, enabling relationship-aware bundles ([graph.rs:1-132]).
- Auto-eviction keeps the SQLite database near the configured ceiling by removing the coldest chunks/nodes when `autoEvict` and `maxDatabaseSizeBytes` are provided ([ingest.rs:725-779]).
- `warm_up_embedder` exposes one-shot model initialization so long-lived agents can prime embeddings up front ([ingest.rs:1310-1340]).

## Semantic Lookup & Bundling

- `semantic_search` opens the SQLite database read-only, resolves the desired embedding model, streams chunk embeddings, and maintains a top-k heap. Returned chunks carry surrounding context and trigger `UPDATE file_chunks SET hits = hits + 1` so usage influences eviction ([search.rs:1-231]).
- `context_bundle` assembles file metadata, symbol definitions, graph neighbors, and related snippets. It now memoizes responses by file hash, selector, ranges, and budget so repeat queries avoid duplicate work, and its multi-tier trimming falls back from full text to focused excerpts and summaries while surfacing explicit token-usage guidance (default 3 000 tokens or `INDEX_MCP_BUDGET_TOKENS`) ([bundle.rs:1-314,586-899]).
- `code_lookup` inside `service.rs` routes `mode="search"` requests to semantic search and `mode="bundle"` to contextual bundles, mirroring the legacy “auto” router ([service.rs:293-341]).

## Freshness & History Tracking

- `index_status` tallies files, chunks, graph nodes, embeddings, and ingestion history, then compares the stored `commit_sha` against the current HEAD to compute an `is_stale` flag ([index_status.rs:1-166]).
- `repository_timeline` shells out to `git log`, normalizes relative `since` expressions, captures churn statistics, diff previews, top files, and directory summaries, and persists each commit to `repository_timeline_entries` for later retrieval ([git_timeline.rs:1-954]).
- `repository_timeline_entry_detail` reloads cached commits (including stored diffs) when a client drills into a specific SHA ([git_timeline.rs:309-394]).

## Runtime & Operations

- `main.rs` provides a CLI with logging controls (including optional file logging), working-directory overrides, and a `--watch` mode that schedules background ingests with debounce, quiet, and alternate-database options ([main.rs:1-178]).
- `watcher.rs` drives the filesystem watcher: it compiles default include/exclude patterns, de-duplicates change events, and batches path-specific ingests using the same `IngestParams` structure ([watcher.rs:65-269]).
- The helper script `start.sh` (`cargo run --release -p index-mcp-server`) keeps launch ergonomics compatible with prior agents.

## Stored Data Summary

- SQLite tables include `files`, `file_chunks` (with embeddings and `hits`), `code_graph_nodes`, `code_graph_edges`, `ingestions`, `meta`, and `repository_timeline_entries`. Every ingest produces ingestion metrics and updates hit counters that downstream tooling (e.g., auto-eviction and bundle ranking) relies on ([ingest.rs:1089-1290], [git_timeline.rs:900-954]).

With these pieces, the Rust server preserves the “stop blowing context” contract—index everything once, serve tightly scoped bundles, and keep the workspace cache authoritative—while adding native performance, watch-mode ingest, remote tool proxying, and richer git awareness.
