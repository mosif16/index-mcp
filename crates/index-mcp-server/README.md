# index-mcp-server

`index-mcp-server` is a Rust implementation of the Model Context Protocol (MCP) indexing server. It scans a workspace, persists metadata and embeddings to SQLite, and exposes MCP tools for fast semantic code search.

## Features

- Walks project directories and records file metadata in `.mcp-index.sqlite`.
- Generates sentence-transformer embeddings and optional HNSW ANN indexes for hybrid search.
- Serves MCP tools for ingestion, semantic search, code lookup, and repository timelines.

## Usage

Run the server directly with cargo:

```bash
cargo run -p index-mcp-server
```

Add `--watch` to keep the index fresh as files change, or see `./start.sh` in the repository root for additional launch options.

## License

MIT 
