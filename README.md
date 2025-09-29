# index-mcp

An MCP (Model Context Protocol) server that ingests a source-code directory and writes a searchable SQLite index (`.mcp-index.sqlite` by default) into the root of that directory. The index captures file paths, metadata, hashes, and optionally file contents so that downstream MCP clients can query the codebase efficiently.

## Prerequisites

- Node.js 18.17 or newer
- npm 9+

## Install

```bash
npm install
```

## Build

```bash
npm run build
```

The transpiled output is emitted to `dist/`.

### Native acceleration

The Rust module in `crates/index_mcp_native` powers ingestion whenever the bindings load successfully. It parallelizes the filesystem walk, hashing, and text chunking steps so large scans finish much faster. If the addon fails to initialize (or you set `INDEX_MCP_NATIVE_DISABLE=true`), the server falls back to the TypeScript scanner and logs a warning—ingest still works, just without the native speed-up.

`start.sh` rebuilds the addon in release mode on every launch, but you can build it manually if you prefer:

1. Install Rust (`rustup`) and the [`@napi-rs/cli`](https://github.com/napi-rs/napi-rs/tree/main/cli) helper.
2. Build the native addon:

   ```bash
   cd crates/index_mcp_native
   npm install
   npm run build
   ```

3. Restart the MCP server. On startup `ingest_codebase` attempts to load the native scanner; failures fall back to the JavaScript implementation and surface the error through the `info` tool.

## Develop

Use the `dev` script to run the TypeScript entrypoint directly while iterating:

```bash
npm run dev
```

### Watch mode

To keep the SQLite index synchronized while you edit files, launch the watcher-enabled entrypoint:

```bash
npm run watch
```

or pass `--watch` through `npm run dev`:

```bash
npm run dev -- --watch
```

The watcher performs an initial ingest (unless you add `--watch-no-initial`) and then monitors the workspace for file additions, edits, and deletions. Detected changes trigger incremental `ingest_codebase` calls so only the touched paths are reprocessed.

CLI flags such as `--watch-debounce=750`, `--watch-database=.custom-index.sqlite`, `--watch-quiet`, and `--watch-no-initial` control batching, database selection, logging, and the initial full scan. Watcher activity is logged via the shared Pino logger, so status messages land in the same log file instead of stdout by default.

When you embed the server inside another process (for example, automation scripts that spin up multiple instances), `await runCleanup()` from `src/cleanup.ts` once you are done. The cleanup routine stops active watchers, closes stdio transports, drains embedding pipelines before resetting native module caches, and does so asynchronously to avoid lingering memory on repeated runs.

## Run

The server communicates over stdio per the MCP spec. After building, launch it with:

```bash
npm start
```

You can also run the TypeScript source with `npm run dev` during development.

## Codex CLI Setup

A helper script `start.sh` (at the project root) rebuilds the compiled bundles (both the stdio server and the bundled local backend), refreshes the native addon, and then launches everything. Point your Codex CLI config at this script. Adjust the absolute paths to match your environment:

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

For iterative development you can keep the same config and simply point Codex directly at the TypeScript entrypoint instead:

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

Restart your Codex agent after saving the configuration so it picks up the new MCP server.

If you use `agent.toml`, mirror the JSON config and set any log overrides you need. You can paste the block below directly into `~/.config/codex/agent.toml` (swap the absolute path for your checkout):

```toml
[mcp_servers.index_mcp]
command = "/absolute/path/to/index-mcp/start.sh"
env = {
  INDEX_MCP_LOG_LEVEL = "info",
  INDEX_MCP_LOG_DIR = "/absolute/path/to/.index-mcp/logs",
  INDEX_MCP_LOG_CONSOLE = "false"
}
```

The bundled `start.sh` builds on demand, compiles the Rust addon in release mode, launches a lightweight SSE backend at `${LOCAL_BACKEND_HOST:-127.0.0.1}:${LOCAL_BACKEND_PORT:-8765}`, waits for `/healthz`, and finally starts `node dist/server.js`. Adjust the paths and environment variables (including the optional `INDEX_MCP_LOG_*` values) to match your machine.

#### Local backend options

The sidecar backend (`src/local-backend/server.ts`) gives Codex an HTTP/SSE endpoint it can use for health checks or future tool bridging. Tune it with environment variables before invoking `start.sh`:

- `LOCAL_BACKEND_HOST` (default `127.0.0.1`)
- `LOCAL_BACKEND_PORT` (default `8765`)
- `LOCAL_BACKEND_PATH` (default `/mcp`) – SSE subscription path
- `LOCAL_BACKEND_MESSAGES_PATH` (default `/messages`) – POST endpoint used by the SSE transport

Set `INDEX_MCP_NATIVE_DISABLE=true` if you want to force the JavaScript ingestion path even when the native addon is present.

### Remote MCP proxying

Set `INDEX_MCP_REMOTE_SERVERS` to a JSON array describing upstream MCP servers you want to mount locally. Each entry supports namespace configuration, per-request headers, bearer/header-based auth, and retry/backoff options. Example:

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

When the server starts it immediately tries to establish an SSE channel to every remote. Failures are retried in the background without blocking local tools, and any remote tools discovered after a reconnect are added under `${namespace}.${tool}` names automatically. The proxy forwards progress notifications, preserves MCP JSON-RPC framing on stdout, and routes its own logs exclusively to stderr so Codex-friendly transports keep working as-is.


## Exposed tools

### `code_lookup`

Unified entry point that routes repository lookups to the appropriate specialist tool. Provide a `query` to perform semantic search, a `file` (and optional `symbol`) to return a context bundle, or set `mode="graph"` with a `node`/`symbol` descriptor to inspect structural neighbors. When omitted, the server infers the mode in the order `search -> bundle -> graph`. This reduces the amount of tool-selection logic the client needs to learn.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the workspace. |
| `mode` | `'search' \| 'bundle' \| 'graph'` | Override the inferred behavior. |
| `query` | `string` | Natural-language or code query for semantic search. |
| `file` | `string` | Relative file path to summarize via `context_bundle`. |
| `symbol` | `{ name: string, kind?: string, path?: string \| null } \| string` | Optional symbol focus for bundles or graph lookups. Strings are treated as the symbol name. |
| `node` | `{ id?: string, name?: string, kind?: string, path?: string \| null } \| string` | Direct graph node descriptor; strings map to the node name. |
| `direction` | `'incoming' \| 'outgoing' \| 'both'` | Neighbor direction for graph mode (defaults to `outgoing`). |
| `limit` | `number` | Result limit for search or graph queries (defaults to tool-specific values). |
| `maxSnippets` | `number` | Cap the snippets in bundle responses (defaults to 3, max 10). |
| `maxNeighbors` | `number` | Cap related edges in bundle responses (defaults to 12, max 50). |
| `databaseName` | `string` | Optional SQLite filename override. |
| `model` | `string` | Embedding model filter for semantic search when multiple models exist. |

The response includes the resolved mode, a text summary, and the structured payload from the delegated tool (`semantic_search`, `context_bundle`, or `graph_neighbors`).

### `ingest_codebase`

Walks a target directory, stores the metadata and (optionally) UTF-8 content for each file in a SQLite database at the directory root, and prunes entries for files that no longer exist. When GraphRAG extraction is enabled (the default), the chunker also emits structural metadata (imports, classes, functions, and call edges) into dedicated graph tables. The tool now tolerates common parameter aliases (for example `path`, `project_path`, `workspace_root`, `database_path`) and coerces string booleans/integers so agents can supply flexible inputs without failing validation.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `include` | `string[]` | Glob patterns (relative to `root`) to include. Defaults to `**/*`. |
| `exclude` | `string[]` | Glob patterns to exclude. Defaults to common build/system folders and the database file. |
| `databaseName` | `string` | Filename to create in the root (default `.mcp-index.sqlite`). |
| `maxFileSizeBytes` | `number` | Skip files larger than this size (default 8 MiB). |
| `storeFileContent` | `boolean` | When `false`, only metadata is stored; content is omitted. Accepts string booleans (`"true"`/`"false"`). |
| `contentSanitizer` | `{ module: string, exportName?: string, options?: unknown }` | Dynamically import a sanitizer to scrub or redact content before it is persisted. |
| `embedding` | `{ enabled?: boolean, model?: string, chunkSizeTokens?: number, batchSize?: number, chunkOverlapTokens?: number }` | Configure semantic chunking/embedding (defaults use `Xenova/bge-small-en-v1.5` via the Rust `fastembed` pipeline, 256-token chunks, batch size 32). Aliases like `embedding_model`, `chunk_overlap`, and `batch_size` are accepted. |
| `graph` | `{ enabled?: boolean }` | Toggle structural graph extraction (aliases such as `graph_options.active` are supported). Disable if you only need file metadata and embeddings. |
| `paths` | `string[]` | Optional relative paths to re-ingest incrementally (aliases: `target_paths`, `changed_paths`). |

The tool response contains both a human-readable summary and structured content describing the ingestion (file count, skipped files, deleted paths, database size, embedded chunk count, graph node/edge upserts, etc.).

When `paths` are omitted the server inspects MCP request metadata, headers, and environment variables for change lists (for example `MCP_CHANGED_PATHS` or `x-mcp-changed-paths`) and restricts the ingest to those files when possible, reducing unnecessary rescans.

### `semantic_search`

Queries the indexed `file_chunks` table using cosine similarity between the stored embeddings and a user-supplied query string. Results surface the best-matching snippets along with scores and metadata. The parameter parser accepts aliases such as `text`/`search_query`, `database_path`, and coercible numeric strings for `limit`.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. Should match the path used for ingestion. |
| `query` (required) | `string` | Natural-language or code-oriented search string. |
| `databaseName` | `string` | Override the SQLite filename if you changed it during ingestion. |
| `limit` | `number` | Max matches to return (default 8, capped at 50). |
| `model` | `string` | Optional embedding model identifier when multiple models are stored in the database (alias: `embedding_model`). |

Each response includes the evaluated chunk count, the embedding model used, and the top-ranked snippets with metadata (path, chunk index, score, sanitized content, byte offsets, line spans, and nearby context before/after the match).

### `graph_neighbors`

Queries the GraphRAG side index populated during ingestion to surface structural relationships (imports and call edges) around a specific node. Supports aliases such as `target`, `entity`, `file_path`, `edge_direction`, and `max_neighbors` to make agent usage more forgiving.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `node` (required) | `{ id?: string, path?: string \| null, kind?: string, name: string }` | Identifies which graph node to inspect. Provide an `id` for an exact match, or combine `name` with `path`/`kind`. |
| `direction` | `'incoming' \| 'outgoing' \| 'both'` | Edge direction to fetch (defaults to `'outgoing'`). Accepts alias `edge_direction`. |
| `limit` | `number` | Maximum edges to return (default 16, capped at 100); alias `max_neighbors` is also handled. |
| `databaseName` | `string` | Override the SQLite filename if you changed it during ingestion. |

The response echoes the resolved node and lists neighbors with edge metadata (direction, type, line numbers, and any resolved import paths), enabling multi-hop traversal strategies.

### `context_bundle`

Returns a single payload that combines file metadata, discovered definitions, related graph edges, and representative content snippets. Use it when an MCP client needs a concise “cheat sheet” for a file or a specific symbol without issuing multiple tool calls.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `file` (required) | `string` | Relative path (POSIX) to the file to summarize. Aliases such as `file_path` and `target_path` are accepted. |
| `symbol` | `{ name: string, kind?: string } \| string` | Optional symbol selector. When provided, `context_bundle` highlights the matching graph node and filters related edges. |
| `maxSnippets` | `number` | Maximum snippets to return (default 3, capped at 10). |
| `maxNeighbors` | `number` | Maximum related edges per direction (default 12, capped at 50). |
| `databaseName` | `string` | Override the SQLite filename if you changed it during ingestion. |

Snippets come from embedded chunks when available; otherwise the tool falls back to stored file content so clients always receive at least one preview. Warnings surface when graph metadata or snippets are missing, helping agents decide whether a fresh ingest is needed.

### `indexing_guidance_tool`

Returns the same reminders surfaced by the `indexing_guidance` prompt, but wrapped as a tool response for clients that do not expose prompt invocation yet. The tool accepts no parameters.

| Field | Type | Description |
|-------|------|-------------|
| `guidance` | `string` | Full text of the current indexing reminders (mirrors the prompt output). |

MCP clients can surface this string directly or store it for quick reference next to the other tool results. The textual summary included in the MCP response simply notes that guidance is available; use the structured `guidance` field for the complete instructions.

### `index_status`

Surfaces high-signal diagnostics about the on-disk SQLite index so agents can understand how fresh and complete their context is before issuing expensive search calls. The tool reports database size, total files/chunks captured, available embedding models, graph coverage, and the most recent ingestion runs.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `databaseName` | `string` | Override the SQLite filename if you changed it during ingestion. |
| `historyLimit` | `number` | Maximum number of recent ingestions to return (default 5, capped at 25). Accepts aliases like `history_limit` and `recent_runs`. |

The response includes a concise summary plus a structured payload with database metadata and an array of recent ingestions (each showing timestamps, file counts, skipped/deleted entries, and duration). When the database is missing, the tool explicitly instructs the caller to run `ingest_codebase` first.

### `info`

Reports MCP server diagnostics: the dynamically loaded package name/version, active instruction banner, native addon health, and basic runtime details. Use it to verify deployments before calling heavier tools.

The tool accepts no parameters and responds with a structured payload containing:

| Field | Description |
|-------|-------------|
| `name`, `version`, `description` | Values read directly from `package.json`, so releases always surface the current version. |
| `instructions` | The server instructions broadcast to clients during connection. |
| `nativeModule` | `{ status: 'ready' \| 'unavailable' \| 'error', message?: string }` describing the Rust addon status. |
| `environment` | `nodeVersion`, `platform`, `cwd`, and `pid` for quick troubleshooting. |

## Project structure

```
├── src/
│   ├── changed-paths.ts     # Resolve diff-based ingest scopes from request metadata
│   ├── cleanup.ts           # Shared cleanup registry triggered on shutdown
│   ├── constants.ts         # Shared defaults for tool configuration
│   ├── context-bundle.ts    # Compose context bundle responses
│   ├── embedding.ts         # Lightweight transformer utilities for embeddings
│   ├── graph-query.ts       # GraphRAG neighbor lookup helper
│   ├── graph.ts             # Structural metadata extraction helpers
│   ├── ingest.ts            # Code ingestion, chunking, graph extraction, SQLite persistence
│   ├── input-normalizer.ts  # Alias/coercion helpers for tool parameters
│   ├── local-backend/
│   │   └── server.ts        # HTTP/SSE helper launched by start.sh
│   ├── logger-config.ts     # Resolve log directory, level, and diagnostics
│   ├── logger.ts            # Pino logger configured for file + optional console output
│   ├── native/
│   │   ├── fallback.ts      # JavaScript fallback when native bindings are unavailable
│   │   └── index.ts         # Native module loader and status helpers
│   ├── package-metadata.ts  # Lazy loader for name/version/description from package.json
│   ├── remote-proxy.ts      # Remote MCP registration and retry handling
│   ├── root-resolver.ts     # Resolve workspace roots from metadata, headers, env
│   ├── search.ts            # Semantic retrieval over stored embeddings
│   ├── server.ts            # MCP server wiring, tool registration, prompts
│   ├── status.ts            # Inspect SQLite index coverage and ingestion history
│   ├── types/
│   │   └── native.ts        # Shared types for native bindings
│   └── watcher.ts           # File-watcher daemon for incremental ingests
├── crates/index_mcp_native/ # Rust addon sources + build tooling
├── dist/                    # Build output (generated)
├── package.json
├── tsconfig.json
└── eslint.config.js
```

## Notes

- Files larger than `maxFileSizeBytes` are skipped to avoid ballooning the index. Adjust per codebase needs.
- Binary files are detected heuristically (null-byte scan) and stored without content even when `storeFileContent` is true.
- The ingestion table keeps track of added, updated, and deleted entries so repeated runs stay fast, and unchanged files are skipped using mtime/size checks.
- When the native addon is available, ingestion now performs a metadata-only scan first and only re-reads files whose size or mtime changed. Large repos with minimal edits avoid rehashing unchanged files, dramatically reducing IO and memory pressure.
- Provide a sanitizer module to strip secrets or redact sensitive payloads before they reach the index.
- Semantic embeddings (BGE base) are computed for sanitized text chunks by default; disable or tune chunking via the `embedding` ingest option if you need to trade accuracy for speed.
- Structural metadata is stored in `code_graph_nodes` and `code_graph_edges` tables to power GraphRAG queries; disable via `graph.enabled = false` if you only need file/embedding data.
- Patterns from a root `.gitignore` file are honored automatically so ignored artifacts never enter the index.
- Runtime logs are emitted via Pino to `~/.index-mcp/logs/server.log` by default. Tune with `INDEX_MCP_LOG_DIR`, `INDEX_MCP_LOG_FILE`, `INDEX_MCP_LOG_LEVEL`, and set `INDEX_MCP_LOG_CONSOLE=true` to mirror logs to stdout/stderr.
- Embedding models cache to `~/.index-mcp/models` by default (override with `INDEX_MCP_MODEL_CACHE_DIR` or `FASTEMBED_CACHE_DIR`) so multiple workspaces reuse downloads instead of spawning per-directory `model_cache` folders.
- The MCP server reads its name/version/description from `package.json` at startup, so updating the package version automatically flows through to tool metadata and the `info` response.
- Automated regression tests have been removed; there is currently no supported `npm test` workflow or CI suite.

## Accelerating with Rust

Large repositories can overwhelm the single-threaded ingestion pipeline. See [docs/rust-acceleration.md](docs/rust-acceleration.md) for a roadmap that introduces a `napi-rs` native module to parallelize filesystem crawling, chunking, and graph extraction while keeping the existing JavaScript fallback path intact. The plan covers crate layout, runtime feature flags, and CI packaging so the Rust acceleration can ship incrementally.

## Troubleshooting

- Ensure the `root` directory exists and is readable; the tool throws an error otherwise.
- Delete the generated database file (`.mcp-index.sqlite` by default) if you need to reset the index from scratch.
