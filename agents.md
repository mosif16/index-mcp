# Codex MCP Best Practices

Transport
	•	Use stdio as the primary method. Codex CLI reliably supports stdio today.
	•	If a server is HTTP/SSE-only, connect through a stdio↔HTTP/SSE proxy.

Framing
	•	Stdout must contain newline-delimited JSON-RPC only.
	•	All logs and diagnostics go to stderr.

Configuration
	•	Use ~/.codex/config.toml with a top-level [mcp_servers.<name>] table.
	•	Required keys: command, args (optional), env.
	•	Consider setting startup_timeout_ms for servers with slow startup.

Security
	•	Secrets go into environment variables, passed as headers inside the server/proxy.
	•	Enforce TLS for non-localhost servers; allow plain HTTP only with explicit opt-in.
	•	Validate Origin headers to guard against DNS rebinding.

Streaming
	•	When using Streamable HTTP with SSE, maintain message ordering and preserve JSON-RPC IDs.
	•	Implement reconnects with backoff and jitter.

Observability
	•	Bound buffers and enforce backpressure policies.
	•	Use structured logs on stderr with fields like component, remote, event, latency_ms.

Backward Compatibility
	•	Ensure local MCP tools behave the same when no remote is mounted.
	•	Namespace mirrored tools from remote servers, e.g. remoteName.tool_id.

Testing Checklist
	•	Stdout purity (no stray logs).
	•	Cold start within startup_timeout_ms.
	•	Streaming resilience: reconnect after stream failure.
	•	Config validation: TOML structure correct and recognized by Codex.



# MCP Agent Guide

This guide covers the Rust implementation of **index-mcp**. Codex CLI loads this file automatically at session start, so the goal is to provide concise, actionable steps for connecting, indexing a project and querying the resulting SQLite snapshot.

## 1. Overview

- **Purpose:** build a SQLite index (`.mcp-index.sqlite` by default) that maps relative paths to metadata, hashes and optional file contents so tools can retrieve information quickly.
- **Primary tool:** `ingest_codebase` — walk the workspace and create the database.
- **Lookup tool:** `code_lookup` — fetch a file by path or search for a substring across stored content.
- **Status tool:** `index_status` — report the latest ingest summary.
- **Prompt:** `indexing_guidance` — reminder prompt describing when to re-run ingestion.
- **Entrypoint:** `start.sh` — builds the Rust binary (release by default) and launches the stdio server.

## 2. Prerequisites

- Rust toolchain (stable 1.75+ recommended)
- `cargo` on the `PATH`
- macOS or Linux environment

No Node.js or npm dependencies are required.

## 3. Build

From the repository root:

```bash
cargo build --release
```

The binary is written to `target/release/index-mcp`. The helper script `start.sh` performs this step automatically unless `INDEX_MCP_SKIP_BUILD=1` is set.

## 4. Configuration

Point your MCP client at `start.sh`. Example TOML for Codex CLI:

```toml
[mcp_servers.index_mcp]
command = "/Users/mohammedsayf/Desktop/index-mcp/start.sh"
startup_timeout_sec = 30
tool_timeout_sec = 300

[mcp_servers.index_mcp.env]
INDEX_MCP_LOG_LEVEL = "INFO"
INDEX_MCP_WORKING_DIR = "/Users/mohammedsayf/Desktop/index-mcp"
INDEX_MCP_DATABASE_NAME = ".mcp-index.sqlite"
```

Environment variables:

- `INDEX_MCP_WORKING_DIR` — overrides the process working directory for relative roots.
- `INDEX_MCP_DATABASE_NAME` — custom SQLite filename.
- `INDEX_MCP_LOG_LEVEL` — one of `error`, `warn`, `info`, `debug`, `trace`.
- `INDEX_MCP_BUILD_PROFILE` — `release` (default) or another Cargo profile name.

## 5. Typical Workflow

1. Launch the server via `start.sh` or `cargo run --release`.
2. Call `ingest_codebase` with `{ "root": "." }` to build the initial snapshot.
3. Use `code_lookup` with `path` to retrieve file contents or with `query` to perform a substring search.
4. Use `index_status` to confirm the timestamp, file count and database location of the last ingest.
5. Re-run `ingest_codebase` after modifying files so the SQLite snapshot stays fresh.

## 6. Tool Parameters

`ingest_codebase` recognises:

- `root` — absolute or relative path (defaults to server working directory).
- `include` / `exclude` — arrays of glob patterns (default excludes `.git`, `target`, `node_modules`, `dist`, temporary directories and the SQLite file itself).
- `databaseName` — overrides the SQLite filename.
- `maxFileSizeBytes` — skip files larger than this limit (default 8 MiB).
- `storeFileContent` — when `false`, only metadata is stored.

`code_lookup` recognises:

- `path` — relative path inside the indexed workspace.
- `query` — substring to search for within stored content.
- `limit` — maximum number of results when searching (default 10).

`index_status` takes no parameters.

## 7. Database Schema

`files(path TEXT PRIMARY KEY, size INTEGER, modified INTEGER, hash TEXT, content TEXT)`

`ingestions(id TEXT PRIMARY KEY, root TEXT, started_at INTEGER, finished_at INTEGER, file_count INTEGER, skipped_count INTEGER, store_content INTEGER)`

Every call to `ingest_codebase` replaces the entire snapshot atomically. The database lives at `root/databaseName`.

## 8. Troubleshooting

- **Missing binary:** run `cargo build --release` manually or ensure `start.sh` can download the Rust toolchain.
- **Index absent:** run `ingest_codebase` before calling `code_lookup` or `index_status`.
- **Binary file skipped:** increase `maxFileSizeBytes` or leave `storeFileContent` at `true` so text files are captured.
- **Custom root:** pass an absolute path in `root` or set `INDEX_MCP_WORKING_DIR` in the environment.

---

Update this guide whenever CLI arguments, environment variables or tool behaviour changes.
