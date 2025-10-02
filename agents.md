Keep the rust-migration.md updated. The legacy `docs/` directory was removed; shared design notes now live alongside the sources at the repository root.

# Codex MCP Best Practices

Transport
	•	Expose MCP servers over stdio whenever possible; fall back to a stdio↔HTTP/SSE bridge only when a remote must stay on HTTP.
	•	Keep stdout JSON-RPC only. Send logs, tracing, and diagnostics to stderr.

Framing
	•	Codex CLI expects newline-delimited JSON responses. Guard against accidental buffering or multiplexed stdout/stderr streams.
	•	Apply backpressure – bound queues, drop noisy events, and prefer structured logs.

Configuration
	•	Configure servers in `~/.codex/config.toml` under `[mcp_servers.<name>]` with `command`, optional `args`, and `env`.
	•	Tune `startup_timeout_sec` for slower binaries (Rust builds) and `tool_timeout_sec` for long-running ingests or history queries.

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
	•	Track ingest harness RSS; the streaming batch pipeline should stay near 2 GB versus the former 13 GB baseline.
	•	Exercise reconnection logic for SSE bridges.
	•	Verify `config.toml` changes are picked up after agent restarts.



# MCP Agent Guide

This guide explains how to run the **index-mcp** Rust server with Codex CLI (or any MCP-compatible client). The Rust binary is now the only runtime; the former Node/TypeScript entrypoint has been removed.

## 1. Overview

- **Purpose:** Build and query a `.mcp-index.sqlite` database that captures file metadata, embeddings, and git history so agents can answer questions without re-parsing the repo.
- **Primary runtime:** `crates/index-mcp-server` – a Rust binary using the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK.
- **Helper script:** `start.sh` launches the Rust binary via `cargo run`, honouring `INDEX_MCP_ARGS` and `INDEX_MCP_CARGO_PROFILE` overrides.
- **Watch mode:** `cargo run -p index-mcp-server -- --watch` (or `INDEX_MCP_ARGS="--watch" ./start.sh`) keeps the SQLite index fresh after file edits.
- **Remote MCP proxy:** Configure `INDEX_MCP_REMOTE_SERVERS` with JSON descriptors to mount additional MCP tools behind the Rust server.

## 2. Prerequisites

- Rust toolchain **1.76+** (via `rustup`).
- `cargo` on the `PATH` (required by `start.sh`).
- SQLite runtime libraries (bundled through `rusqlite` when using the `bundled` feature).
Optional utilities: `sqlite3` CLI for ad-hoc inspection and `watchexec`/`entr` when scripting ingestion outside the built-in watcher.

Verify your toolchain before wiring the server into an agent:

```bash
rustup show active-toolchain
rustc --version
cargo --version
```

Run `rustup update` periodically so long-lived agents benefit from compiler and dependency fixes.

## 3. Build and Run

Common commands from the repository root:

```
cargo check -p index-mcp-server                 # Compile without running
cargo run -p index-mcp-server                   # Launch Rust server over stdio
cargo run -p index-mcp-server -- --watch        # Watch mode (defaults: 500 ms debounce)
cargo run -p index-mcp-server -- --help         # List CLI flags

# Via the helper script
./start.sh                                      # cargo run --release -p index-mcp-server
INDEX_MCP_ARGS="--watch --watch-no-initial" ./start.sh
INDEX_MCP_CARGO_PROFILE=debug ./start.sh        # override cargo profile
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

`start.sh` is a thin wrapper around `cargo run`. It selects the requested cargo profile, forwards
`INDEX_MCP_ARGS` after `--`, and leaves logging to existing environment variables.

## 4. Tools and Prompts

The Rust binary registers the full tool surface that previously lived in the Node server.

| Tool / Prompt | Notes |
|---------------|-------|
| `ingest_codebase` | Walks the workspace, respects `.gitignore`, stores metadata, embeddings, and auto-evicts least-used chunks when requested. |
| `semantic_search` | Embedding-powered chunk retrieval with language guesses, context padding, and hit counters. |
| `code_lookup` | Routes `mode="search"` queries to semantic search and `mode="bundle"` to context bundles. |
| `context_bundle` | Returns file metadata, focus definitions, nearby snippets, and quick links within a token budget. |
| `index_status` | Summarizes index freshness, embedding models, ingestion history, and git parity. |
| `repository_timeline` | Streams recent git commits with churn stats, directory highlights, optional diffs, and PR URLs. |
| `repository_timeline_entry` | Recovers cached commit details and (when available) full diff text for a specific SHA. |
| `indexing_guidance` / `indexing_guidance_tool` | Prompt and tool variants for ingest reminders. |
| Remote proxies | Any remote declared in `INDEX_MCP_REMOTE_SERVERS` is namespaced and surfaced alongside local tools. |

The server banner reminds clients to re-run `ingest_codebase` after edits, check `index_status` when unsure about freshness, and prefer `code_lookup` for discovery.

## 5. Typical Workflow

1. **Ingest first** – `ingest_codebase { "root": "." }` (or `--watch`) before any lookup. Honor `.gitignore`, skip files larger than 8 MiB, and enable `autoEvict`/`maxDatabaseSizeBytes` when the SQLite file grows.
2. **Verify freshness** – Run `index_status` whenever the agent is unsure about recency; re-run `ingest_codebase` immediately if it reports staleness.
3. **Default to `code_lookup`** – Use `query="..."` for semantic discovery and `file="..."` plus an optional symbol for bundles before escalating to other tools.
4. **Review repo history** – Call `repository_timeline` (and `repository_timeline_entry`) prior to planning or applying code changes so diffs stay in view.
5. **Refine context** – Reach for `semantic_search` follow-ups and `context_bundle` for focused file context while staying within `INDEX_MCP_BUDGET_TOKENS` or an explicit `budgetTokens` value.
6. **Keep the index current** – Re-run `ingest_codebase` after edits or rely on watch mode so the database mirrors the workspace.
7. **Budget & diagnostics** – Set `INDEX_MCP_BUDGET_TOKENS`, tune tool limits, and use `info`/`indexing_guidance` when you need diagnostics or reminders.

## 6. SQLite Layout

The Rust runtime preserves the schema introduced by the legacy implementation:

- `files` – path, size, modified time (ms), SHA-256 hash, stored content, last indexed timestamp.
- `file_chunks` – chunk text, embeddings (float32 blobs), byte/line spans, hit counters, embedding model id.
- `ingestions` – ingest history, durations, counts, and root paths.
- `meta` – key/value store for commit SHA, last indexed timestamp, and other metadata.

Databases created before the rewrite remain compatible with the current runtime.

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

- **Missing toolchain:** `start.sh` aborts when `cargo` is absent. Install Rust via `rustup` and re-run the script.
- **Slow cold start:** Use `INDEX_MCP_CARGO_PROFILE=debug` while iterating; switch back to release for production agents.
- **Cold ingest latency:** Startup now warms the quantized `Xenova/all-MiniLM-L6-v2` embedder in the background, trimming the first `ingest_codebase` on clean workspaces to ~24s; subsequent runs reuse the cache and finish in milliseconds when files are unchanged.
- **Embedding download issues:** The server uses `fastembed`; failures leave the cache empty. Re-run once connectivity returns or disable embeddings with `{ "embedding": { "enabled": false } }`.
- **SQLite locks:** The Rust ingestor uses transactions with `PRAGMA foreign_keys=ON`. If another process holds the DB, re-run after releasing the lock or configure a different database filename.
- **Watcher noise:** Add `--watch-quiet` or tighten `--watch-debounce`. The watcher respects include/exclude globs plus `.gitignore` entries.
- **Remote proxy errors:** Check stderr for reconnect attempts. Invalid auth headers or TLS failures propagate as MCP tool errors.

## 9. Additional Resources

- `README.md` – High-level features and CLI examples (see "Docs relocation" for context on the removed `docs/` folder).
- `rust-migration.md` – Detailed migration status and parity checklist (relocated from `docs/`).
- `rust-acceleration.md` – Design notes for native ingestion (relocated from `docs/`).
- `start.sh` – Runtime launcher with environment variable overrides.

Keep this guide in sync with feature changes; update it whenever new tools ship or runtime defaults change.
