USE NODE-RUNTIME BRANCH. MAIN BRANCH UNDER WORK 



# index-mcp

An MCP (Model Context Protocol) server that scans a source-code workspace and builds a searchable SQLite index (`.mcp-index.sqlite` by default) in the project root. The index stores file metadata, hashes, and optionally file contents so MCP-compatible clients can perform fast lookups, semantic search, and graph exploration.

**New in this version:** Context budget control and hotness tracking to prevent overwhelming LLMs with excessive context. Only small, focused bundles are sent based on actual usage patterns.

## Key capabilities

- **Fast ingestion** – Uses a native Rust addon (when available) to parallelize filesystem walking, hashing, and chunking.
- **Flexible querying** – Downstream MCP clients can retrieve semantic chunks, structural graph edges, or full file context.
- **Incremental updates** – Watch mode keeps the SQLite database aligned with live edits.
- **Optional remotes** – Proxy additional MCP servers and expose them under configurable namespaces.
- **Token budget control** – Context bundles respect token limits to avoid sending excessive data to LLMs.
- **Hotness tracking** – Tracks which symbols and snippets are actually used, with optional eviction of stale data.
- **Freshness checks** – Compares current git commit with indexed commit to detect when re-indexing is needed.

## Requirements

- Rust toolchain (`cargo`) 1.75+ (default runtime)
- Node.js **18.17 or newer** and npm **9+** (needed for the legacy Node runtime and native addon builds)

Optional but recommended:

- [`@napi-rs/cli`](https://github.com/napi-rs/napi-rs/tree/main/cli) to compile the native addon manually when using the Node runtime

## Quick start

### Run the Rust server (default)

```bash
cargo run -p index-mcp-server --release
```

Omit `--release` for faster incremental builds or pass additional CLI flags with
`INDEX_MCP_ARGS="--watch --watch-debounce=250" ./start.sh`.

### Legacy Node runtime

Install dependencies and build the TypeScript bundles:

```bash
npm install
npm run build
```

Launch the compiled stdio server:

```bash
npm start
```

During development you can run the TypeScript entrypoint directly:

```bash
npm run dev
```

## Native acceleration (Node runtime)

When running the legacy Node server, the native module in `crates/index_mcp_native` is loaded automatically on startup. When present it accelerates ingestion; when it fails to load (or if you set `INDEX_MCP_NATIVE_DISABLE=true`) the server falls back to the TypeScript implementation and logs a warning.

To build the addon manually:

```bash
cd crates/index_mcp_native
npm install
npm run build
```

Restart the server after rebuilding—the next `ingest_codebase` call will attempt to load the native scanner and report issues through the `info` tool.

## Watch mode and cleanup

### Rust runtime

The Rust binary ships with a built-in watcher. Prefix your launch command with the desired flags:

```bash
cargo run -p index-mcp-server --release -- --watch --watch-debounce=250
```

Or, when using `start.sh`, populate `INDEX_MCP_ARGS`:

```bash
INDEX_MCP_ARGS="--watch --watch-debounce=250" ./start.sh
```

Flags mirror the Node watcher (`--watch-root`, `--watch-no-initial`, `--watch-quiet`, `--watch-database`).

### Node runtime

Keep the SQLite index synchronized with local edits by enabling the watcher:

```bash
npm run watch
```

You can also pass flags through `npm run dev` (e.g. `npm run dev -- --watch`). Useful options:

- `--watch-debounce=<ms>` – Adjust debounce before re-ingesting (default 500 ms).
- `--watch-database=<filename>` – Choose a custom SQLite filename.
- `--watch-no-initial` – Skip the initial full ingest.
- `--watch-quiet` – Silence watcher logs.

When embedding the Node server in another process, call `await runCleanup()` from `src/cleanup.ts` before exit so watchers, transports, and embedding pipelines shut down cleanly.

## Context Budget and Hotness Tracking

This implementation ensures that **raw index data is not sent to the LLM**. Instead, everything is stored in SQLite, and only small, focused bundles are sent when requested.

### Token Budget Control

Context bundles automatically respect token limits to prevent overwhelming LLMs:

- Default budget: **3000 tokens** (configurable via `INDEX_MCP_BUDGET_TOKENS` env var)
- Prioritizes key definitions first, then nearby lines
- Trims content to fit within budget
- Warns when content is truncated

```bash
export INDEX_MCP_BUDGET_TOKENS=5000  # Increase to 5000 tokens
```

### Hotness Tracking

The system tracks which symbols and snippets are actually accessed:

- Every served symbol/snippet increments a `hits` counter
- Optional automatic eviction of least-used data when database exceeds size limit (default: 150 MB)
- Keeps high-value, frequently accessed data in the index

To enable automatic eviction during ingest:

```typescript
await ingestCodebase({
  root: '/path/to/repo',
  autoEvict: true,
  maxDatabaseSizeBytes: 100 * 1024 * 1024  // 100 MB limit
});
```

### Freshness Checks

The `index_status` tool now compares the current git commit with the indexed commit:

- Returns `isStale: true` if commits don't match
- Includes both current and indexed commit SHAs
- Tracks when indexing occurred

This allows agents to automatically detect when re-indexing is needed after git operations.

## Rust runtime

The Rust MCP server in `crates/index-mcp-server` now serves the full tool surface—ingest, semantic
search, context bundles, graph neighbors, repository timelines, remote MCP proxying, prompts, and
watch mode—using the official [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk).

Build or run the Rust binary with:

```bash
cargo check -p index-mcp-server
cargo run -p index-mcp-server --release
```

The executable communicates over stdio, so it can be registered with MCP-compatible clients like
Claude Desktop in the same way as the Node entrypoint. Use `INDEX_MCP_ARGS` (or pass CLI flags
directly) to enable watcher mode, override the working directory, or tweak debounce settings.

## Codex CLI setup

The project root includes a `start.sh` helper. By default it compiles and launches the Rust binary,
passing through any `INDEX_MCP_*` environment variables. Set `INDEX_MCP_RUNTIME=node` to boot the
legacy JavaScript stack instead (which rebuilds `dist/`, refreshes the native addon, and starts the
local backend before launching `node dist/server.js`). Point your Codex configuration at this script
and customize paths for your machine:

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "/absolute/path/to/index-mcp/start.sh",
      "env": {
        "INDEX_MCP_LOG_LEVEL": "info",
        "INDEX_MCP_LOG_DIR": "/absolute/path/to/.index-mcp/logs",
        "INDEX_MCP_LOG_CONSOLE": "false",
        "INDEX_MCP_BUDGET_TOKENS": "3000"
      }
    }
  }
}
```

For live development you can reference the TypeScript entrypoint directly:

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "npx",
      "args": ["tsx", "src/server.ts"],
      "env": {
        "INDEX_MCP_LOG_LEVEL": "debug",
        "INDEX_MCP_LOG_CONSOLE": "true",
        "INDEX_MCP_REMOTE_SERVERS": "[]"
      }
    }
  }
}
```

A matching `agent.toml` configuration looks like:

```toml
[mcp_servers.index_mcp]
command = "/absolute/path/to/index-mcp/start.sh"
env = {
  INDEX_MCP_LOG_LEVEL = "info",
  INDEX_MCP_LOG_DIR = "/absolute/path/to/.index-mcp/logs",
  INDEX_MCP_LOG_CONSOLE = "false",
  INDEX_MCP_BUDGET_TOKENS = "3000"
}
```

Set `INDEX_MCP_RUNTIME = "node"` in either configuration if you prefer the TypeScript server.

### Local backend options

When `INDEX_MCP_RUNTIME=node`, `start.sh` launches a lightweight HTTP/SSE backend implemented in
`src/local-backend/server.ts`. Configure it with environment variables before invoking the script:

- `LOCAL_BACKEND_HOST` (default `127.0.0.1`)
- `LOCAL_BACKEND_PORT` (default `8765`)
- `LOCAL_BACKEND_PATH` (default `/mcp`) – SSE subscription path
- `LOCAL_BACKEND_MESSAGES_PATH` (default `/messages`) – POST endpoint for the SSE transport
- `INDEX_MCP_NATIVE_DISABLE=true` – Force the JavaScript ingestion path when debugging native issues

### Remote MCP proxying

Expose additional MCP servers through this instance by setting `INDEX_MCP_REMOTE_SERVERS` to a JSON array. Each entry supports namespace configuration, authentication, and retry controls. Example:

```bash
export INDEX_MCP_REMOTE_SERVERS='[
  {
    "name": "search-backend",
    "namespace": "remote.search",
    "url": "https://example.com/mcp",
    "auth": { "type": "bearer", "tokenEnv": "REMOTE_SEARCH_TOKEN" },
    "retry": { "maxAttempts": Infinity, "initialDelayMs": 500, "maxDelayMs": 30000 }
  }
]'
```

On startup the server opens SSE channels to each remote. Connection failures retry in the background without blocking local tools, and remote tool namespaces are added automatically when a connection succeeds.

## Troubleshooting tips

- **Missing `better_sqlite3` binding:** Ensure `npm install` ran under Node ≥ 18 so prebuilt binaries download successfully.
- **Native addon issues:** Set `INDEX_MCP_NATIVE_DISABLE=true` to force the TypeScript ingest path while you debug, then rebuild the addon when ready.
- **Index not updating:** Re-run `ingest_codebase` or enable watch mode after making changes.
- **Embedding download hiccups:** Rerun the ingest after network connectivity is restored or temporarily disable embeddings with `{"embedding": {"enabled": false}}`.
- **Large repositories:** Increase `maxFileSizeBytes` or adjust `include`/`exclude` patterns.
- **Missing bundles:** Run `npm run build` if `start.sh` reports a missing `dist/` output.

## Additional resources

- `docs/` – Reference material for Codex CLI integration and MCP best practices.
- `docs/rust-migration.md` – Status tracker for the ongoing Rust server migration.
- `agents.md` – High-level guidance for running the MCP server alongside Codex.
