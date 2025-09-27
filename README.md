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

## Develop

Use the `dev` script to run the TypeScript entrypoint directly while iterating:

```bash
npm run dev
```

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

Walks a target directory, stores the metadata and (optionally) UTF-8 content for each file in a SQLite database at the directory root, and prunes entries for files that no longer exist.

| Argument | Type | Description |
|----------|------|-------------|
| `root` (required) | `string` | Absolute or relative path to the codebase root. |
| `include` | `string[]` | Glob patterns (relative to `root`) to include. Defaults to `**/*`. |
| `exclude` | `string[]` | Glob patterns to exclude. Defaults to common build/system folders and the database file. |
| `databaseName` | `string` | Filename to create in the root (default `.mcp-index.sqlite`). |
| `maxFileSizeBytes` | `number` | Skip files larger than this size (default 512 KiB). |
| `storeFileContent` | `boolean` | When `false`, only metadata is stored; content is omitted. |
| `contentSanitizer` | `{ module: string, exportName?: string, options?: unknown }` | Dynamically import a sanitizer to scrub or redact content before it is persisted. |

The tool response contains both a human-readable summary and structured content describing the ingestion (file count, skipped files, deleted paths, database size, etc.).

## Project structure

```
├── src/
│   ├── ingest.ts      # Code ingestion and SQLite persistence
│   └── server.ts      # MCP server wiring and tool registration
├── dist/              # Build output (generated)
├── package.json
├── tsconfig.json
└── eslint.config.js
```

## Notes

- Files larger than `maxFileSizeBytes` are skipped to avoid ballooning the index. Adjust per codebase needs.
- Binary files are detected heuristically (null-byte scan) and stored without content even when `storeFileContent` is true.
- The ingestion table keeps track of added, updated, and deleted entries so repeated runs stay fast, and unchanged files are skipped using mtime/size checks.
- Provide a sanitizer module to strip secrets or redact sensitive payloads before they reach the index.
- Patterns from a root `.gitignore` file are honored automatically so ignored artifacts never enter the index.

## Troubleshooting

- Ensure the `root` directory exists and is readable; the tool throws an error otherwise.
- Delete the generated database file (`.mcp-index.sqlite` by default) if you need to reset the index from scratch.
