# MCP Agent Guide

This document explains how to run and use the **index-mcp** server with the Codex CLI (or any MCP-compatible client). It covers installation, configuration, exposed tools, recommended workflows, and troubleshooting.

## 1. Overview

- **Purpose:** Index a codebase into a root-level SQLite database (`.mcp-index.sqlite`) so agents can perform fast metadata/content queries.
- **Primary tool:** `ingest_codebase` – scans files, stores hashes, metadata, and optionally UTF-8 content.
- **Supporting prompt:** `indexing_guidance` – returns reminders about when to run the ingestor.
- **Helper script:** `start.sh` – builds the project (if needed) and launches the MCP server over stdio.

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

This installs dependencies including `@modelcontextprotocol/sdk`, `better-sqlite3@^12.4.1`, `fast-glob`, and TypeScript tooling.

## 4. Build and Development Scripts

| Command              | Description                                                     |
|----------------------|-----------------------------------------------------------------|
| `npm run dev`        | Run `src/server.ts` via `tsx` for live development.             |
| `npm run build`      | Clean and transpile TypeScript to `dist/`.                      |
| `npm start`          | Execute the compiled server (`dist/server.js`) with Node.       |
| `npm run lint`       | ESLint (flat config) over the workspace.                        |
| `npm run clean`      | Remove `dist/`.                                                 |

`start.sh` wraps the build/start workflow so external agents don’t have to worry about the build step.

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

### Prompts

| Name                | Description |
|---------------------|-------------|
| `indexing_guidance` | Returns a short reminder about available tools and when to re-run `ingest_codebase`. |

### Server Instructions Banner

When the client connects, it receives this message:

> Tools available: ingest_codebase (index the current codebase into SQLite) and indexing_guidance (prompt describing when to reindex). Always run ingest_codebase on a new or freshly checked out codebase before asking for help. Any time you or the agent edits files, re-run ingest_codebase so the SQLite index stays current.

## 7. Typical Workflow

1. **Start the MCP server** via Codex (`start.sh` handles build & launch).
2. **Initial indexing:** call `ingest_codebase` with `{"root": "."}` (or another path) before requesting analysis.
3. **Perform agent tasks** (editing files, searching, etc.).
4. **Re-index after edits:** call `ingest_codebase` again so `.mcp-index.sqlite` reflects the latest changes.
5. **Optional inspection:** use `sqlite3 .mcp-index.sqlite` to run ad-hoc queries if needed.

## 8. Database Schema (summary)

- `files`
  - `path` (PRIMARY KEY) – POSIX-style relative path
  - `size` (bytes)
  - `modified` (mtime in ms)
  - `hash` (SHA-256 of file contents)
  - `last_indexed_at` (timestamp in ms)
  - `content` (nullable TEXT; omitted for large/binary files when `storeFileContent=false`)
- `ingestions`
  - `id` (UUID)
  - `root` (absolute root path)
  - `started_at`, `finished_at`
  - `file_count`, `skipped_count`, `deleted_count`

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

During repeat runs the ingestor compares size + mtime against the existing database, reusing prior entries when nothing changed and skipping unnecessary file reads. A `.gitignore` located at the root is parsed automatically so ignored paths never enter the index.

The tool response returns both text (a summary) and `structuredContent` matching the `ingestToolOutputShape` schema.

## 10. Troubleshooting

- **Node binding error:** If the MCP server complains about missing `better_sqlite3.node`, ensure `npm install` was run with a Node version >= 18. Prebuilt binaries are available beginning with `better-sqlite3@12.x`.
- **Index not updating:** Re-run `ingest_codebase` as soon as files are added/changed. The server reminder text emphasizes this.
- **Large repos:** Increase `maxFileSizeBytes` or adjust `include`/`exclude` to limit scope.
- **Manual rebuild:** `npm run build` regenerates `dist/` if `start.sh` ever reports a missing bundle.

## 11. Next Enhancements

- Add a dedicated search tool (`search_codebase`) to query `.mcp-index.sqlite` without dropping to a shell.
- Optional compression or pruning strategies for very large codebases.

---

For questions or improvements, update this document alongside corresponding code changes.
