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

CLI flags such as `--watch-debounce=750`, `--watch-database=.custom-index.sqlite`, `--watch-quiet`, and `--watch-no-initial` control batching, database selection, logging, and the initial full scan.

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
      "command": "/Users/mohammedsayf/Desktop/index-mcp/start.sh"
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
      "cwd": "/Users/mohammedsayf/Desktop/index-mcp"
    }
  }
}
```

Restart your Codex agent after saving the configuration so it picks up the new MCP server.

If you maintain your Codex CLI config in `agent.toml`, mirror the same setup with TOML syntax:

```toml
[mcp_servers.index_mcp]
command = "/Users/mohammedsayf/Desktop/index-mcp/start.sh"
env = { LOG_LEVEL = "INFO" }
```

The bundled `start.sh` handles building on demand and then spawns `node dist/server.js`. Adjust the paths and environment variables for your machine.


## Exposed tools

### `ingest_codebase`

Walks a target directory, stores the metadata and (optionally) UTF-8 content for each file in a SQLite database at the directory root, and prunes entries for files that no longer exist. When GraphRAG extraction is enabled (the default), the chunker also emits structural metadata (imports, classes, functions, and call edges) into dedicated graph tables.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `include` | `string[]` | Glob patterns (relative to `root`) to include. Defaults to `**/*`. |
| `exclude` | `string[]` | Glob patterns to exclude. Defaults to common build/system folders and the database file. |
| `databaseName` | `string` | Filename to create in the root (default `.mcp-index.sqlite`). |
| `maxFileSizeBytes` | `number` | Skip files larger than this size (default 512 KiB). |
| `storeFileContent` | `boolean` | When `false`, only metadata is stored; content is omitted. |
| `contentSanitizer` | `{ module: string, exportName?: string, options?: unknown }` | Dynamically import a sanitizer to scrub or redact content before it is persisted. |
| `embedding` | `{ enabled?: boolean, model?: string, chunkSizeTokens?: number, batchSize?: number, chunkOverlapTokens?: number }` | Configure semantic chunking/embedding (defaults use `Xenova/bge-small-en-v1.5`, 256-token chunks). |
| `graph` | `{ enabled?: boolean }` | Toggle structural graph extraction. Disable if you only need file metadata and embeddings. |
| `paths` | `string[]` | Optional relative paths to re-ingest incrementally (useful for watcher-driven updates). |

The tool response contains both a human-readable summary and structured content describing the ingestion (file count, skipped files, deleted paths, database size, embedded chunk count, graph node/edge upserts, etc.).

### `semantic_search`

Queries the indexed `file_chunks` table using cosine similarity between the `bge-small-en-v1.5` embeddings stored during ingestion and a user-supplied query string. Results surface the best-matching snippets along with scores and metadata.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. Should match the path used for ingestion. |
| `query` (required) | `string` | Natural-language or code-oriented search string. |
| `databaseName` | `string` | Override the SQLite filename if you changed it during ingestion. |
| `limit` | `number` | Max matches to return (default 8, capped at 50). |
| `model` | `string` | Optional embedding model identifier when multiple models are stored in the database. |

Each response includes the evaluated chunk count, the embedding model used, and the top-ranked snippets with metadata (path, chunk index, score, sanitized content, byte offsets, line spans, and nearby context before/after the match).

### `graph_neighbors`

Queries the GraphRAG side index populated during ingestion to surface structural relationships (imports and call edges) around a specific node.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `node` (required) | `{ id?: string, path?: string \| null, kind?: string, name: string }` | Identifies which graph node to inspect. Provide an `id` for an exact match, or combine `name` with `path`/`kind`. |
| `direction` | `'incoming' \| 'outgoing' \| 'both'` | Edge direction to fetch (defaults to `'outgoing'`). |
| `limit` | `number` | Maximum edges to return (default 16, capped at 100). |
| `databaseName` | `string` | Override the SQLite filename if you changed it during ingestion. |

The response echoes the resolved node and lists neighbors with edge metadata (direction, type, line numbers, and any resolved import paths), enabling multi-hop traversal strategies.

## Project structure

```
├── src/
│   ├── constants.ts    # Shared defaults for tool configuration
│   ├── embedding.ts    # Lightweight transformer utilities for embeddings
│   ├── graph-query.ts  # GraphRAG neighbor lookup helper
│   ├── graph.ts        # Structural metadata extraction helpers
│   ├── ingest.ts       # Code ingestion, chunking, graph extraction, SQLite persistence
│   ├── search.ts       # Semantic retrieval over stored embeddings
│   ├── server.ts       # MCP server wiring and tool registration
│   └── watcher.ts      # File-watcher daemon for incremental ingests
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
- Semantic embeddings (BGE small) are computed for sanitized text chunks by default; disable or tune chunking via the `embedding` ingest option if you need to trade accuracy for speed.
- Structural metadata is stored in `code_graph_nodes` and `code_graph_edges` tables to power GraphRAG queries; disable via `graph.enabled = false` if you only need file/embedding data.
- Patterns from a root `.gitignore` file are honored automatically so ignored artifacts never enter the index.

## Accelerating with Rust

Large repositories can overwhelm the single-threaded ingestion pipeline. See [docs/rust-acceleration.md](docs/rust-acceleration.md) for a roadmap that introduces a `napi-rs` native module to parallelize filesystem crawling, chunking, and graph extraction while keeping the existing JavaScript fallback path intact. The plan covers crate layout, runtime feature flags, and CI packaging so the Rust acceleration can ship incrementally.

## Troubleshooting

- Ensure the `root` directory exists and is readable; the tool throws an error otherwise.
- Delete the generated database file (`.mcp-index.sqlite` by default) if you need to reset the index from scratch.
