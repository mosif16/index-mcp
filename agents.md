# MCP Agent Guide

This document explains how to run and use the **index-mcp** server with the Codex CLI (or any MCP-compatible client). It covers installation, configuration, exposed tools, recommended workflows, and troubleshooting.

## 1. Overview

- **Purpose:** Index a codebase into a root-level SQLite database (`.mcp-index.sqlite`) so agents can perform fast metadata/content queries.
- **Primary tool:** `ingest_codebase` – scans files, stores hashes, metadata, and optionally UTF-8 content.
- **Supporting prompt:** `indexing_guidance` – returns reminders about when to run the ingestor.
- **Helper script:** `start.sh` – builds the project (if needed) and launches the MCP server over stdio.
- **GraphRAG side index:** Structural relationships (imports, functions, calls) are captured automatically and can be explored via `graph_neighbors`.
- **Watch mode:** `npm run watch` streams incremental ingests so the SQLite database stays aligned with live edits.

## 2. Prerequisites

- Node.js **18.17+** (current development has been validated on Node 24.7.0).
- npm **9+**.
- macOS or Linux shell environment (tested on macOS).
- `sqlite3` CLI (optional but handy for manual database inspection).

## 3. Installation

From the project root (`/Users/mohammedsayf/Desktop/index-mcp`):

```bash
npm install
```

This installs dependencies including `@modelcontextprotocol/sdk`, `better-sqlite3@^12.4.1`, `fast-glob`, `chokidar`, `ignore`, `zod`, `@xenova/transformers`, and TypeScript tooling.

## 4. Build and Development Scripts

| Command              | Description                                                     |
|----------------------|-----------------------------------------------------------------|
| `npm run dev`        | Run `src/server.ts` via `tsx` for live development.             |
| `npm run build`      | Clean and transpile TypeScript to `dist/`.                      |
| `npm start`          | Execute the compiled server (`dist/server.js`) with Node.       |
| `npm run watch`      | Run the server with an incremental ingest file watcher.         |
| `npm run lint`       | ESLint (flat config) over the workspace.                        |
| `npm run clean`      | Remove `dist/`.                                                 |

`start.sh` wraps the build/start workflow so external agents don’t have to worry about the build step.

### Watch Mode Flags

`npm run watch` runs `tsx src/server.ts --watch` under the hood. Optional flags exposed by the server:

- `--watch-root <path>` — override the directory to watch (defaults to the process CWD).
- `--watch-debounce <ms>` — adjust the debounce window before an ingest is triggered (default 500 ms, minimum 50 ms).
- `--watch-no-initial` — skip the initial full ingest when the watcher starts.
- `--watch-quiet` — silence watcher log output.
- `--watch-database <filename>` — customize the SQLite filename instead of `.mcp-index.sqlite`.

These align with the `startIngestWatcher` options and make it easy to tune incremental ingest behaviour for larger projects.

## 5. Codex CLI Configuration

### JSON (`~/.config/codex/agent.json`)

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "/Users/mohammedsayf/Desktop/index-mcp/start.sh"
    }
  }
}
```

### TOML (`~/.config/codex/agent.toml`)

```toml
[mcp_servers.index_mcp]
command = "/Users/mohammedsayf/Desktop/index-mcp/start.sh"
env = { LOG_LEVEL = "INFO" }
```

After editing the config, restart the Codex CLI agent so the new MCP server registers.

## 6. Exposed Tools and Prompts

### Tools

| Name             | Type  | Description |
|------------------|-------|-------------|
| `ingest_codebase` | Tool  | Walks a directory, stores metadata + optional UTF-8 content for each file in `.mcp-index.sqlite`, and prunes deleted entries. Accepts optional glob include/exclude, custom database name, file-size limits, and `storeFileContent` toggle. |
| `semantic_search` | Tool  | Embedding-powered retrieval across stored `file_chunks` for natural-language or code queries. |
| `graph_neighbors` | Tool  | Query GraphRAG nodes/edges produced during ingestion to inspect imports and call relationships. |

### Prompts

| Name                | Description |
|---------------------|-------------|
| `indexing_guidance` | Returns a short reminder about available tools and when to re-run `ingest_codebase`. |

### Server Instructions Banner

When the client connects, it receives this message:

> Tools available: ingest_codebase (index the current codebase into SQLite), semantic_search (embedding-powered retrieval), graph_neighbors (GraphRAG neighbor explorer), and indexing_guidance (prompt describing when to reindex). Always run ingest_codebase on a new or freshly checked out codebase before asking for help. Any time you or the agent edits files, re-run ingest_codebase so the SQLite index stays current.

## 7. Typical Workflow

1. **Start the MCP server** via Codex (`start.sh` handles build & launch).
2. **Initial indexing:** call `ingest_codebase` with `{"root": "."}` (or another path) before requesting analysis.
3. *(Optional)* **Run the watcher:** `npm run watch` keeps the database fresh by triggering incremental ingests when files change.
4. **Perform agent tasks** (editing files, searching, etc.).
5. **Re-index after edits:** call `ingest_codebase` again (or rely on the watcher) so `.mcp-index.sqlite` reflects the latest changes.
6. **Optional inspection:** use `sqlite3 .mcp-index.sqlite` to run ad-hoc queries if needed.

## 8. Database Schema (summary)

- `files`
  - `path` (PRIMARY KEY) – POSIX-style relative path
  - `size` (bytes)
  - `modified` (mtime in ms)
  - `hash` (SHA-256 of file contents)
  - `last_indexed_at` (timestamp in ms)
  - `content` (nullable TEXT; omitted for large/binary files when `storeFileContent=false`)
- `file_chunks`
  - `id` (UUID PRIMARY KEY)
  - `path` (foreign key -> `files.path`)
  - `chunk_index` (sequential index within the file)
  - `content` (chunk text)
  - `embedding` (BLOB storing normalized float32 vector)
  - `embedding_model` (identifier for the model used)
- `ingestions`
  - `id` (UUID)
  - `root` (absolute root path)
  - `started_at`, `finished_at`
  - `file_count`, `skipped_count`, `deleted_count`
- `code_graph_nodes`
  - `id` (stable hash PRIMARY KEY)
  - `path` (nullable file path; null for external modules/symbols)
  - `kind` (e.g., `file`, `function`, `module`, `method`)
  - `name`, `signature`, `range_start`, `range_end`
  - `metadata` (JSON string with line info, class name, etc.)
- `code_graph_edges`
  - `id` (stable hash PRIMARY KEY)
  - `source_id`, `target_id` (foreign keys -> `code_graph_nodes.id`)
  - `type` (`imports` or `calls`)
  - `source_path`, `target_path`
  - `metadata` (JSON with import specifiers, call locations, etc.)

## 9. Common Options for `ingest_codebase`

| Field              | Default               | Notes |
|--------------------|-----------------------|-------|
| `root`             | none (required)       | Path to scan. Relative paths resolve against the server’s working directory. |
| `include`          | `["**/*"]`           | Glob patterns to include (fast-glob syntax). |
| `exclude`          | Several defaults      | Includes VCS folders, `node_modules`, `dist`, and the database itself. You can pass extra patterns. |
| `databaseName`     | `.mcp-index.sqlite`   | File created at the root. |
| `maxFileSizeBytes` | `524288` (512 KiB)    | Larger files are skipped and logged in `skipped`. |
| `storeFileContent` | `true`                | If `false`, only metadata is stored. Binary detection uses a null-byte heuristic. |
| `contentSanitizer` | `undefined`           | Optional `{ module, exportName?, options? }` descriptor that loads a sanitizer to redact or strip contents before storage. |
| `embedding`        | defaults enabled       | Configure semantic chunking (`enabled`, `model`, `chunkSizeTokens`, `chunkOverlapTokens`, `batchSize`). |
| `graph`            | `true`                 | Toggle structural graph extraction (`{ enabled?: boolean }`). |
| `paths`            | `undefined`            | Provide specific relative paths to update incrementally (skips scanning untouched files). |

During repeat runs the ingestor compares size + mtime against the existing database, reusing prior entries when nothing changed and skipping unnecessary file reads. A `.gitignore` located at the root is parsed automatically so ignored paths never enter the index.

Relative `root` values are resolved against the caller-supplied working directory metadata (such as `_meta.cwd` or the `x-mcp-cwd` header) when available, so CLI clients can safely pass `"."` to target their active workspace. If no caller context is provided the path falls back to the server process directory.

Embeddings default to the `Xenova/bge-small-en-v1.5` model provided by `@xenova/transformers`. The server downloads and caches the model on first use; set `embedding.model` in tool inputs if you need an alternative.

The tool response returns both text (a summary) and `structuredContent` matching the `ingestToolOutputShape` schema.

## 10. Troubleshooting

- **Node binding error:** If the MCP server complains about missing `better_sqlite3.node`, ensure `npm install` was run with a Node version >= 18. Prebuilt binaries are available beginning with `better-sqlite3@12.x`.
- **Index not updating:** Re-run `ingest_codebase` as soon as files are added/changed. The server reminder text emphasizes this.
- **Repeated ingest failures after a transient embedding error:** The embedding pipeline cache in `src/embedding.ts` stores the first failed promise. If the initial model download fails (e.g., due to a network hiccup), every subsequent `ingest_codebase` call will reuse that rejected promise and fail until the process restarts. Restart the MCP server or set `{"embedding": {"enabled": false}}` on the tool call as a stopgap, then rerun ingest once connectivity is stable.
- **Large repos:** Increase `maxFileSizeBytes` or adjust `include`/`exclude` to limit scope.
- **Manual rebuild:** `npm run build` regenerates `dist/` if `start.sh` ever reports a missing bundle.

## 11. Next Enhancements

- Expand GraphRAG queries (e.g., multi-hop traversals and filtering by edge type).
- Explore compression or pruning strategies for very large codebases.
- Surface watcher status/metrics over an MCP prompt for remote monitoring.

---

For questions or improvements, update this document alongside corresponding code changes.
