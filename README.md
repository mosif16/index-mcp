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

The Rust module in `crates/index_mcp_native` now powers every ingest run. It parallelizes the filesystem walk, hashing, and text chunking steps. The JavaScript fallback has been removed, so the native addon must be available before the server starts.

1. Install Rust (`rustup`) and the [`@napi-rs/cli`](https://github.com/napi-rs/napi-rs/tree/main/cli) helper.
2. Build the native addon:

   ```bash
   cd crates/index_mcp_native
   npm install
   npm run build
   ```

3. Restart the MCP server. On startup `ingest_codebase` attempts to load the native scanner and will throw if the addon cannot be initialized. Rebuild the crate if you see a load error.

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

## Run

The server communicates over stdio per the MCP spec. After building, launch it with:

```bash
npm start
```

You can also run the TypeScript source with `npm run dev` during development.

## Codex CLI Setup

A helper script `start.sh` (at the project root) ensures the TypeScript is built before launching the server. Point your Codex CLI config at this script. Adjust the absolute paths to match your environment:

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "/absolute/path/to/index-mcp/start.sh"
    }
  }
}
```

During iterative development you can swap the command for `npx tsx` and point at `src/server.ts` instead of the built artifact:

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "npx",
      "args": [
        "tsx",
        "src/server.ts"
      ],
      "cwd": "/absolute/path/to/index-mcp"
    }
  }
}
```

Restart your Codex agent after saving the configuration so it picks up the new MCP server.

If you use `agent.toml`, mirror the JSON config and set any log overrides you need:

```toml
[mcp_servers.index_mcp]
command = "/absolute/path/to/index-mcp/start.sh"
env = {
  INDEX_MCP_LOG_LEVEL = "info",
  INDEX_MCP_LOG_DIR = "/absolute/path/to/.index-mcp/logs",
  INDEX_MCP_LOG_CONSOLE = "false"
}
```

The bundled `start.sh` builds on demand, verifies the native addon, and then launches `node dist/server.js`. Adjust the paths and environment variables (including the optional `INDEX_MCP_LOG_*` values) to match your machine.


## Exposed tools

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
| `embedding` | `{ enabled?: boolean, model?: string, chunkSizeTokens?: number, batchSize?: number, chunkOverlapTokens?: number }` | Configure semantic chunking/embedding (defaults use `Xenova/all-MiniLM-L6-v2`, 256-token chunks). Aliases like `embedding_model`, `chunk_overlap`, and `batch_size` are accepted. |
| `graph` | `{ enabled?: boolean }` | Toggle structural graph extraction (aliases such as `graph_options.active` are supported). Disable if you only need file metadata and embeddings. |
| `paths` | `string[]` | Optional relative paths to re-ingest incrementally (aliases: `target_paths`, `changed_paths`). |

The tool response contains both a human-readable summary and structured content describing the ingestion (file count, skipped files, deleted paths, database size, embedded chunk count, graph node/edge upserts, etc.).

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
│   ├── constants.ts        # Shared defaults for tool configuration
│   ├── embedding.ts        # Lightweight transformer utilities for embeddings
│   ├── graph-query.ts      # GraphRAG neighbor lookup helper
│   ├── graph.ts            # Structural metadata extraction helpers
│   ├── ingest.ts           # Code ingestion, chunking, graph extraction, SQLite persistence
│   ├── input-normalizer.ts # Alias/coercion helpers for tool parameters
│   ├── logger.ts           # Pino logger configured for file + optional console output
│   ├── package-metadata.ts # Lazy loader for name/version/description from package.json
│   ├── status.ts           # Inspect existing SQLite indexes for coverage and ingestion history
│   ├── search.ts           # Semantic retrieval over stored embeddings
│   ├── server.ts           # MCP server wiring and tool registration
│   └── watcher.ts          # File-watcher daemon for incremental ingests
├── dist/               # Build output (generated)
├── package.json
├── tsconfig.json
└── eslint.config.js
```

## Notes

- Files larger than `maxFileSizeBytes` are skipped to avoid ballooning the index. Adjust per codebase needs.
- Binary files are detected heuristically (null-byte scan) and stored without content even when `storeFileContent` is true.
- The ingestion table keeps track of added, updated, and deleted entries so repeated runs stay fast, and unchanged files are skipped using mtime/size checks.
- Provide a sanitizer module to strip secrets or redact sensitive payloads before they reach the index.
- Semantic embeddings (BGE base) are computed for sanitized text chunks by default; disable or tune chunking via the `embedding` ingest option if you need to trade accuracy for speed.
- Structural metadata is stored in `code_graph_nodes` and `code_graph_edges` tables to power GraphRAG queries; disable via `graph.enabled = false` if you only need file/embedding data.
- Patterns from a root `.gitignore` file are honored automatically so ignored artifacts never enter the index.
- Runtime logs are emitted via Pino to `~/.index-mcp/logs/server.log` by default. Tune with `INDEX_MCP_LOG_DIR`, `INDEX_MCP_LOG_FILE`, `INDEX_MCP_LOG_LEVEL`, and set `INDEX_MCP_LOG_CONSOLE=true` to mirror logs to stdout/stderr.
- The MCP server reads its name/version/description from `package.json` at startup, so updating the package version automatically flows through to tool metadata and the `info` response.

## Accelerating with Rust

Large repositories can overwhelm the single-threaded ingestion pipeline. See [docs/rust-acceleration.md](docs/rust-acceleration.md) for a roadmap that introduces a `napi-rs` native module to parallelize filesystem crawling, chunking, and graph extraction while keeping the existing JavaScript fallback path intact. The plan covers crate layout, runtime feature flags, and CI packaging so the Rust acceleration can ship incrementally.

## Troubleshooting

- Ensure the `root` directory exists and is readable; the tool throws an error otherwise.
- Delete the generated database file (`.mcp-index.sqlite` by default) if you need to reset the index from scratch.
