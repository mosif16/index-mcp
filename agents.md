Keep the rust-migration.md updated

# Codex MCP Best Practices

Transport
	•	Expose MCP servers over stdio whenever possible; fall back to a stdio↔HTTP/SSE bridge only when a remote must stay on HTTP.
	•	Keep stdout JSON-RPC only. Send logs, tracing, and diagnostics to stderr.

Framing
	•	Codex CLI expects newline-delimited JSON responses. Guard against accidental buffering or multiplexed stdout/stderr streams.
	•	Apply backpressure – bound queues, drop noisy events, and prefer structured logs.

Configuration
	•	Configure servers in `~/.codex/config.toml` under `[mcp_servers.<name>]` with `command`, optional `args`, and `env`.
	•	Tune `startup_timeout_sec` for slower binaries (Rust builds) and `tool_timeout_sec` for long-running ingests or graph traversals.

Security
	•	Inject secrets through environment variables or MCP headers; never bake them into binaries or config files.
	•	Require TLS for any remote MCP host that is not `localhost`.
	•	Validate `Origin` and `Host` headers inside HTTP/SSE bridges to stop rebinding attacks.

Streaming
	•	When proxying to SSE transports, preserve event order and IDs. Reconnect with exponential backoff.

Backward Compatibility
	•	Mirror remote tool names under a namespace (for example `docs.search`) so local clients avoid collisions.
	•	Keep behaviour consistent whether or not remotes are mounted.

Testing Checklist
	•	Validate stdout purity.
	•	Confirm cold-start latency stays within configured timeouts.
	•	Exercise reconnection logic for SSE bridges.
	•	Verify `config.toml` changes are picked up after agent restarts.



# MCP Agent Guide

This guide explains how to run the **index-mcp** Rust server with Codex CLI (or any MCP-compatible client). The Rust binary now provides the primary runtime; the Node/TypeScript entrypoint remains available as a fallback for older workflows.

## 1. Overview

- **Purpose:** Build and query a `.mcp-index.sqlite` database that captures file metadata, embeddings, graph edges, and git history so agents can answer questions without re-parsing the repo.
- **Primary runtime:** `crates/index-mcp-server` – a Rust binary using the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK.
- **Helper script:** `start.sh` launches the preferred runtime. By default it executes the Rust binary via `cargo run`; set `INDEX_MCP_RUNTIME=node` to fall back to the Node server.
- **Watch mode:** `cargo run -p index-mcp-server -- --watch` (or `INDEX_MCP_ARGS="--watch" ./start.sh`) keeps the SQLite index fresh after file edits.
- **Remote MCP proxy:** Configure `INDEX_MCP_REMOTE_SERVERS` with JSON descriptors to mount additional MCP tools behind the Rust server.

## 2. Prerequisites

- Rust toolchain **1.76+** (via `rustup`).
- `cargo` on the `PATH` (required by `start.sh`).
- SQLite runtime libraries (bundled through `rusqlite` when using the `bundled` feature).
- Node.js **>=18** and npm **>=9** only if you need the legacy Node runtime or build tooling.

Optional utilities: `sqlite3` CLI for ad-hoc inspection and `watchexec`/`entr` when scripting ingestion outside the built-in watcher.

## 3. Build and Run

Common commands from the repository root:

```
cargo check -p index-mcp-server                 # Compile without running
cargo run -p index-mcp-server                   # Launch Rust server over stdio
cargo run -p index-mcp-server -- --watch        # Watch mode (defaults: 500 ms debounce)
cargo run -p index-mcp-server -- --help         # List CLI flags

# Via the helper script (defaults to Rust runtime)
./start.sh                                      # cargo run --release -p index-mcp-server
INDEX_MCP_ARGS="--watch --watch-no-initial" ./start.sh
INDEX_MCP_CARGO_PROFILE=debug ./start.sh        # override cargo profile
INDEX_MCP_RUNTIME=node ./start.sh               # force Node fallback
```

Notable CLI flags:

| Flag | Description |
|------|-------------|
| `--cwd <path>` | Override process working directory before booting the server. |
| `--watch` | Enable filesystem watcher ingest loop. |
| `--watch-root <path>` | Override watch root (defaults to `cwd`). |
| `--watch-debounce <ms>` | Adjust debounce (minimum 50 ms). |
| `--watch-no-initial` | Skip the initial full ingest when watcher starts. |
| `--watch-quiet` | Silence watcher progress logs. |
| `--watch-database <name>` | Use a custom database filename in watch mode. |

`start.sh` defers to `cargo` when `INDEX_MCP_RUNTIME` is unset or `rust`. For Node compatibility, the script still rebuilds `dist/` and the local backend before executing `node dist/server.js`.

## 4. Tools and Prompts

The Rust binary registers the full tool surface exposed by the Node server.

| Tool / Prompt | Notes |
|---------------|-------|
| `ingest_codebase` | Walks the workspace, respects `.gitignore`, stores metadata, embeddings, and graph edges, supports incremental `paths`, and optional auto-eviction. |
| `semantic_search` | Embedding-powered chunk retrieval with language guesses, classification, context padding, and hit counters. |
| `code_lookup` | Routes `mode="search"` queries to semantic search and `mode="bundle"` to context bundles. |
| `context_bundle` | Returns file metadata, focus definitions, docstrings, TODO counts, graph neighbors, snippets, and quick links. |
| `graph_neighbors` | Expands GraphRAG nodes (imports/calls) in either direction with optional depth constraints. |
| `index_status` | Summarizes index freshness, embedding models, ingestion history, and git parity. |
| `repository_timeline` | Streams recent git commits with churn stats, directory highlights, optional diffs, and PR URLs. |
| `indexing_guidance` / `indexing_guidance_tool` | Prompt and tool variants for ingest reminders. |
| Remote proxies | Any remote declared in `INDEX_MCP_REMOTE_SERVERS` is namespaced and surfaced alongside local tools. |

The server banner reminds clients to re-run `ingest_codebase` after edits, check `index_status` when unsure about freshness, and prefer `code_lookup` for discovery.

## 5. Typical Workflow

1. **Initial ingest** – `ingest_codebase { "root": "." }` (or rely on `--watch`). Honor `.gitignore` to avoid bloating the database.
2. **Verify freshness** – `index_status` reports missing ingests, schema mismatches, or git drift.
3. **Discover context** – `code_lookup` (search/bundle), `semantic_search`, or `context_bundle` supply snippets and structured metadata.
4. **Explore structure** – `graph_neighbors` for GraphRAG hops, `repository_timeline` for recent commits and diff summaries.
5. **Budget management** – Set `INDEX_MCP_BUDGET_TOKENS` (or pass `budgetTokens`) so responses fit within downstream context limits.
6. **Remote tooling** – Configure `INDEX_MCP_REMOTE_SERVERS` (JSON array) to mount remote MCP endpoints; the Rust proxy maintains SSE connections, retries with exponential backoff, and mirrors tool metadata under `<namespace>.*` names.
7. **Re-ingest after changes** – Re-run `ingest_codebase` or let watch mode feed the database incremental updates.

## 6. SQLite Layout

The Rust and Node runtimes share the same schema:

- `files` – path, size, modified time (ms), SHA-256 hash, stored content, last indexed timestamp.
- `file_chunks` – chunk text, embeddings (float32 blobs), byte/line spans, hit counters, embedding model id.
- `ingestions` – ingest history, durations, counts, and root paths.
- `code_graph_nodes` / `code_graph_edges` – TypeScript graph metadata (imports, calls, visibility, signatures, docstrings).
- `meta` – key/value store for commit SHA, last indexed timestamp, and other metadata.

Databases created by either runtime remain interchangeable.

## 7. Remote MCP Proxying

Set `INDEX_MCP_REMOTE_SERVERS` to a JSON array describing remotes, for example:

```
[
  {
    "name": "docs",
    "url": "https://example.com/mcp",
    "headers": { "x-api-key": "${DOCS_KEY}" },
    "namespace": "docs",
    "retry": { "maxAttempts": 5, "initialDelayMs": 500, "backoffMultiplier": 2.0 }
  }
]
```

Tokens can be sourced from environment variables (for example `${DOCS_KEY}`) or dedicated `auth` blocks. The proxy surfaces remote tools as `<namespace>.<tool>` and automatically tears down and reconnects on failures.

## 8. Troubleshooting

- **Missing toolchain:** `start.sh` aborts when `cargo` is absent. Install Rust via `rustup` or set `INDEX_MCP_RUNTIME=node`.
- **Slow cold start:** Use `INDEX_MCP_CARGO_PROFILE=debug` while iterating; switch back to release for production agents.
- **Embedding download issues:** Both runtimes use `fastembed`; failures leave the cache empty. Re-run once connectivity returns or disable embeddings with `{ "embedding": { "enabled": false } }`.
- **SQLite locks:** The Rust ingestor uses transactions with `PRAGMA foreign_keys=ON`. If another process holds the DB, re-run after releasing the lock or point each runtime at a separate database filename.
- **Watcher noise:** Add `--watch-quiet` or tighten `--watch-debounce`. The watcher respects include/exclude globs plus `.gitignore` entries.
- **Remote proxy errors:** Check stderr for reconnect attempts. Invalid auth headers or TLS failures propagate as MCP tool errors.

## 9. Additional Resources

- `README.md` – High-level features and CLI examples.
- `docs/rust-migration.md` – Detailed migration status and parity checklist.
- `docs/rust-acceleration.md` – Design notes for native ingestion.
- `start.sh` – Runtime launcher with environment variable overrides.
- `crates/index_mcp_native` – Shared ingestion logic (Rust addon) reused by both runtimes.

Keep this guide in sync with feature changes; update it whenever new tools ship or runtime defaults change.
