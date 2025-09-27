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

Add the server to your Codex CLI agent configuration (typically `~/.config/codex/agent.json`). Adjust the absolute paths to match your environment:

```json
{
  "mcpServers": {
    "index-mcp": {
      "command": "node",
      "args": [
        "/absolute/path/to/index-mcp/dist/server.js"
      ],
      "cwd": "/absolute/path/to/index-mcp"
    }
  }
}
```

During iterative development you can swap the command for `tsx` and point at `src/server.ts` instead of the built artifact:

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
- The ingestion table keeps track of added, updated, and deleted entries so repeated runs stay fast.

## Troubleshooting

- Ensure the `root` directory exists and is readable; the tool throws an error otherwise.
- Delete the generated database file (`.mcp-index.sqlite` by default) if you need to reset the index from scratch.

