# index-mcp

An MCP (Model Context Protocol) server that scans a source-code workspace and builds a searchable SQLite index (`.mcp-index.sqlite` by default) in the project root. The index stores file metadata, hashes, and optionally file contents so MCP-compatible clients can perform fast lookups, semantic search, and graph exploration.

## Key capabilities

- **Fast ingestion** – Uses a native Rust addon (when available) to parallelize filesystem walking, hashing, and chunking.
- **Flexible querying** – Downstream MCP clients can retrieve semantic chunks, structural graph edges, or full file context.
- **Incremental updates** – Watch mode keeps the SQLite database aligned with live edits.
- **Optional remotes** – Proxy additional MCP servers and expose them under configurable namespaces.

## Requirements

- Node.js **18.17 or newer**
- npm **9+**

Optional but recommended:

- Rust toolchain (`rustup`) for native builds
- [`@napi-rs/cli`](https://github.com/napi-rs/napi-rs/tree/main/cli) to compile the native addon manually

## Quick start

Install dependencies:

```bash
npm install
```

Build the TypeScript bundles (emitted to `dist/`):

```bash
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

## Native acceleration

The native module in `crates/index_mcp_native` is loaded automatically on startup. When present it accelerates ingestion; when it fails to load (or if you set `INDEX_MCP_NATIVE_DISABLE=true`) the server falls back to the TypeScript implementation and logs a warning.

To build the addon manually:

```bash
cd crates/index_mcp_native
npm install
npm run build
```

Restart the server after rebuilding—the next `ingest_codebase` call will attempt to load the native scanner and report issues through the `info` tool.

## Watch mode and cleanup

Keep the SQLite index synchronized with local edits by enabling the watcher:

```bash
npm run watch
```

You can also pass flags through `npm run dev` (e.g. `npm run dev -- --watch`). Useful options:

- `--watch-debounce=<ms>` – Adjust debounce before re-ingesting (default 500 ms).
- `--watch-database=<filename>` – Choose a custom SQLite filename.
- `--watch-no-initial` – Skip the initial full ingest.
- `--watch-quiet` – Silence watcher logs.

When embedding the server in another process, call `await runCleanup()` from `src/cleanup.ts` before exit so watchers, transports, and embedding pipelines shut down cleanly.

## Codex CLI setup

The project root includes a `start.sh` helper that rebuilds both the stdio server bundle and the bundled local backend, refreshes the native addon, waits for the backend health check, and finally launches `node dist/server.js`. Point your Codex configuration at this script and customize paths for your machine:

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "/absolute/path/to/index-mcp/start.sh",
      "env": {
        "INDEX_MCP_LOG_LEVEL": "info",
        "INDEX_MCP_LOG_DIR": "/absolute/path/to/.index-mcp/logs",
        "INDEX_MCP_REMOTE_SERVERS": "[]"
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
  INDEX_MCP_LOG_CONSOLE = "false"
}
```

### Local backend options

`start.sh` launches a lightweight HTTP/SSE backend implemented in `src/local-backend/server.ts`. Configure it with environment variables before invoking the script:

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
- `agents.md` – High-level guidance for running the MCP server alongside Codex.

## Acknowledgments

Created by **msayf** in collaboration with **Record and Learn LLC**. If you find index-mcp helpful, please consider contributing improvements and starring the repository so the community can continue to grow.

