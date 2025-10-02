# index-mcp

`index-mcp` is a Rust-native [Model Context Protocol](https://github.com/modelcontextprotocol) (MCP) server that scans a source-code workspace and writes a searchable SQLite database (`.mcp-index.sqlite`) into the project root. Agents query the database through MCP tools to obtain semantic chunks, graph metadata, or git history without re-reading the entire repository on every request.

The project previously shipped a Node/TypeScript runtime. That implementation has now been retired in favour of the Rust server, which owns the complete tool surface.

## Key Capabilities

- **Fast ingestion** – Parallel filesystem walker with `.gitignore` support, hashing, chunking, embeddings, and optional auto-eviction based on database size targets.
- **Flexible lookups** – `code_lookup`, `semantic_search`, and `context_bundle` expose focused snippets, structured metadata, and graph context for agents.
- **Git awareness** – `repository_timeline` and `repository_timeline_entry` summarise recent commits and cached diffs so agents can reason about repo history.
- **Watch mode** – Optional filesystem watcher re-ingests changed paths automatically for long-running agent sessions.
- **Remote proxies** – Mount additional MCP servers behind the same process by declaring JSON descriptors in `INDEX_MCP_REMOTE_SERVERS`.
- **Context budgeting & hotness tracking** – Bundles respect a configurable token budget and track per-chunk usage to inform eviction heuristics.

## Requirements

- [Rust](https://rustup.rs/) toolchain 1.76 or newer (`cargo` must be on `PATH`).
- SQLite runtime libraries (bundled automatically through `rusqlite` with the `bundled` feature).
- Optional utilities: `sqlite3` CLI for inspection, `watchexec`/`entr` for custom watch workflows.

## Quick Start

Compile or run the server directly with Cargo:

```bash
cargo check -p index-mcp-server              # Compile only
cargo run -p index-mcp-server --release      # Launch the MCP server (release mode)
cargo run -p index-mcp-server -- --help      # Inspect runtime flags

# Smoke test all tools in one go
cargo run -p index-mcp-server --bin ingest_debug --release
```

The repository includes a convenience launcher, `start.sh`, which wraps the same `cargo run` invocation while honouring environment overrides:

```bash
./start.sh                                  # Uses release profile by default
INDEX_MCP_CARGO_PROFILE=debug ./start.sh    # Opt into a custom cargo profile
INDEX_MCP_ARGS="--watch" ./start.sh        # Forward additional CLI flags
```

`INDEX_MCP_ARGS` is tokenised by the shell before being appended after `--` so any runtime flag accepted by the server can be supplied (for example, `--watch-debounce=250`).

## Watch Mode

Enable watch mode either via Cargo directly or with the helper script:

```bash
cargo run -p index-mcp-server --release -- --watch --watch-debounce=250
INDEX_MCP_ARGS="--watch --watch-no-initial" ./start.sh
```

Key flags:

| Flag | Description |
|------|-------------|
| `--cwd <path>` | Override the working directory analysed by the server. |
| `--watch` | Enable the filesystem watcher for incremental ingests. |
| `--watch-root <path>` | Track a directory other than the process `cwd`. |
| `--watch-debounce <ms>` | Tune debounce (minimum 50 ms). |
| `--watch-no-initial` | Skip the initial full ingest on startup. |
| `--watch-quiet` | Silence watcher progress logs. |
| `--watch-database <name>` | Use an alternate SQLite filename for watch mode. |

## Context Budget & Hotness Tracking

Context bundles automatically respect the `INDEX_MCP_BUDGET_TOKENS` environment variable (default: 3000 tokens). Responses prioritise focus definitions, append nearby lines, and truncate intelligently with explicit notices when content is trimmed. Each served chunk increments a `hits` counter which feeds auto-eviction heuristics during ingest.

To cap database size during ingest:

```json
{
  "root": ".",
  "autoEvict": true,
  "maxDatabaseSizeBytes": 150000000
}
```

Pass the payload above to the `ingest_codebase` tool (for example via the MCP client you are integrating with). The server evicts the least-used rows until the size target is met.

## Remote MCP Proxying

Mount additional MCP servers by exporting `INDEX_MCP_REMOTE_SERVERS` before launching the process:

```bash
export INDEX_MCP_REMOTE_SERVERS='[
  {
    "name": "search-backend",
    "namespace": "remote.search",
    "url": "https://example.com/mcp",
    "headers": { "x-api-key": "${SEARCH_TOKEN}" },
    "retry": { "maxAttempts": 5, "initialDelayMs": 500, "backoffMultiplier": 2.0 }
  }
]'
./start.sh
```

Remote tools are surfaced under `<namespace>.<tool>` and benefit from the same structured logging and retry behaviour as the local toolset.

## Troubleshooting

- **Missing toolchain** – Install Rust with `rustup` and ensure `cargo` is on `PATH`.
- **Embedding download issues** – The server uses `fastembed`; transient network failures leave the cache empty. Re-run ingest when connectivity is restored or disable embeddings via `{ "embedding": { "enabled": false } }`.
- **SQLite locks** – Another process may hold the database. Retry after releasing the lock or configure a different database filename with `--watch-database`.
- **Watcher noise** – Increase debounce or enable `--watch-quiet` to reduce log output.

## Further Reading

- `docs/rust-migration.md` – status tracker for the Rust rewrite.
- `docs/rust-acceleration.md` – design notes and benchmarks for the native pipeline.
- `agents.md` – guidance for wiring the server into MCP-compatible clients.
- `IMPLEMENTATION_SUMMARY.md` – historical context for the token-budget and hotness tracking features.
