# index-mcp

`index-mcp` is a Model Context Protocol (MCP) server implemented entirely in Rust. It scans a workspace, records metadata and
(optional) UTF-8 file contents in a SQLite database, and exposes MCP tools for retrieving files or searching previously indexed
content. The server communicates over stdio and is compatible with the [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)
client ecosystem.

## Prerequisites

- Rust 1.75 or newer (the latest stable toolchain is recommended)
- `cargo` on your `PATH`
- SQLite is bundled through the `rusqlite` crate (no system installation required)

## Building

```bash
cargo build --release
```

The compiled binary is written to `target/release/index-mcp`.

## Running

During development you can execute the binary directly:

```bash
cargo run --release
```

For production, invoke the helper script bundled with the repository. It ensures the binary exists (building it when necessary)
and then launches the server over stdio so MCP clients can attach:

```bash
./start.sh
```

The script honours the `INDEX_MCP_BUILD_PROFILE` environment variable (`release` by default) and forwards any additional flags in
`INDEX_MCP_CARGO_FLAGS` to `cargo build`.

### MCP client configuration

Point your MCP client (for example the Codex CLI) at `start.sh`:

```toml
[mcp_servers.index_mcp]
command = "/absolute/path/to/index-mcp/start.sh"
```

Optional environment variables:

- `INDEX_MCP_WORKING_DIR` – override the default workspace (current directory)
- `INDEX_MCP_DATABASE_NAME` – custom SQLite filename (defaults to `.mcp-index.sqlite`)
- `INDEX_MCP_LOG_LEVEL` – `error`, `warn`, `info`, `debug`, or `trace`

## Available tools

### `ingest_codebase`

Walks a repository and writes a fresh SQLite index. Important parameters:

| Parameter | Type | Description |
|-----------|------|-------------|
| `root` | string | Absolute or relative path to the workspace. Defaults to the server working directory. |
| `include` | string[] | Additional glob patterns to include (defaults to `**/*`). |
| `exclude` | string[] | Extra glob patterns to exclude (defaults already skip `.git`, `target`, `node_modules`, etc.). |
| `databaseName` | string | Override the SQLite filename (defaults to `.mcp-index.sqlite`). |
| `maxFileSizeBytes` | integer | Maximum file size to ingest (defaults to 8 MiB). |
| `storeFileContent` | boolean | Store UTF-8 file content alongside metadata (default `true`). |

### `code_lookup`

Queries the previously ingested database. Provide either `path` to retrieve a specific file (including its stored content) or
`query` to search by substring. The optional `limit` parameter caps the number of search matches (default `10`).

### `index_status`

Returns structured information about the most recent ingest run, including file counts, skipped paths and the database location.

## Prompt support

The server exposes a single prompt named `indexing_guidance`. It reminds MCP clients to run `ingest_codebase` after cloning a
repository or modifying files, and to use `code_lookup` or `index_status` once the SQLite snapshot exists.

## Database layout

`ingest_codebase` produces a SQLite database with two tables:

- `files` – path (primary key), size, last-modified timestamp, SHA-256 hash and optional content
- `ingestions` – metadata describing the latest ingest operation (start/end timestamps, file/skip counts, whether content was stored)

Re-running the ingest tool replaces the entire snapshot atomically. The database file lives at the repository root unless a
custom `databaseName` is provided.
