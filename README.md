# index-mcp

`index-mcp` is a Rust-native [Model Context Protocol](https://github.com/modelcontextprotocol) (MCP) server that scans a source-code workspace and writes a searchable SQLite database (`.mcp-index.sqlite`) into the project root. Agents query the database through MCP tools to obtain semantic chunks and git history without re-reading the entire repository on every request.

The project previously shipped a Node/TypeScript runtime. That implementation has now been retired in favour of the Rust server, which owns the complete tool surface. If you need the legacy Node server, check out the `main` branch; everything on `Rust-rewrite` assumes the Rust runtime.

## Key Capabilities

- **Fast ingestion** – Parallel filesystem walker with `.gitignore` support, hashing, chunking, pluggable embeddings (FastEmbed ONNX by default, Candle sentence-transformers optional), and optional auto-eviction based on database size targets. Each chunk now persists summary, symbol, identifier, language, and graph metadata alongside the embedding payload. When embeddings are enabled the ingest pass can also persist an on-disk HNSW index for fast approximate nearest-neighbour (ANN) search.
- **Hybrid lookups** – `semantic_search`, `code_lookup`, and `context_bundle` blend lexical sieves with embedding-backed similarity. Results ship with `source` (`embedding` vs `lexical`), `confidence`, and symbol metadata so agents understand why a match was promoted, while automatically falling back to brute-force scoring if the ANN index is unavailable.
- **Cross-file awareness** – `context_bundle` automatically pulls in graph-linked snippets from neighboring files, annotating each excerpt with edge metadata so downstream prompts can cite related definitions without extra calls.
- **Git awareness** – `repository_timeline` and `repository_timeline_entry` summarise recent commits and cached diffs so agents can reason about repo history.
- **Watch mode** – Optional filesystem watcher re-ingests changed paths automatically for long-running agent sessions.
- **Remote proxies** – Mount additional MCP servers behind the same process by declaring JSON descriptors in `INDEX_MCP_REMOTE_SERVERS`.
- **Context budgeting & observability** – Bundles respect a configurable token budget, track per-chunk usage, and emit diagnostics (model, backend, latency, similarity range) to help tune downstream prompting.

## Requirements

- [Rust](https://rustup.rs/) toolchain 1.76 or newer (`cargo` must be on `PATH`).
- SQLite runtime libraries (bundled automatically through `rusqlite` with the `bundled` feature).
- Optional utilities: `sqlite3` CLI for inspection, `watchexec`/`entr` for custom watch workflows.

## Installing Rust and Cargo

1. **Install `rustup` (recommended path)**
   - macOS / Linux:
     ```bash
     curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
     ```
     macOS users should install the Xcode Command Line Tools first: `xcode-select --install`.
   - Windows: download and run the [`rustup-init.exe`](https://win.rustup.rs/) installer, then follow the prompts (the default "stable" toolchain is fine).
2. **Reload your shell** so `~/.cargo/bin` is on `PATH` (`source ~/.cargo/env` for the current session if needed).
3. **Verify the toolchain**
   ```bash
   rustc --version
   cargo --version
   ```
   Both commands should report versions ≥ `1.76`.
4. **Keep the toolchain current**
   ```bash
   rustup update
   rustup component add clippy rustfmt   # Lints and formatting
   ```
   Install nightly or additional targets (for example `wasm32-wasi`) with `rustup toolchain install` / `rustup target add` if your workflow requires them.

## Cargo Workflow Cheatsheet

- `cargo build` / `cargo build --release` – Compile the project (debug vs. optimised binaries).
- `cargo check` – Type-check quickly without producing binaries.
- `cargo run -p index-mcp-server -- <flags>` – Launch the MCP server with optional CLI flags.
- `cargo test` – Run the Rust test suite (no tests yet, but keep the command handy for future additions).
- `cargo fmt` / `cargo clippy` – Apply formatting and static analysis; recommended before committing changes.

## Quick Start

Compile or run the server directly with Cargo:

```bash
cargo check -p index-mcp-server              # Compile only
cargo run -p index-mcp-server --release      # Launch the MCP server (release mode)
cargo run -p index-mcp-server -- --help      # Inspect runtime flags

# Smoke test all tools in one go
cargo run -p index-mcp-server --bin ingest_debug --release
```

The repository includes a convenience launcher, `start.sh`, which wraps the same `cargo run` invocation while honouring environment overrides and mode presets:

```bash
./start.sh                                  # Production mode (release profile)
./start.sh production                       # Explicit production launch
./start.sh dev                              # Development mode (debug + watcher defaults)
INDEX_MCP_MODE=dev INDEX_MCP_ARGS="--watch-debounce=250" ./start.sh
./start.sh production --watch-quiet         # Mode arg plus extra runtime flags
```

`INDEX_MCP_MODE` or the first positional argument choose between `production` and `development` presets. Additional CLI flags can be provided via `INDEX_MCP_ARGS` or by appending them after the mode; both paths are tokenised before being passed downstream.

## Embedding Backends & ANN Search

`ingest_codebase` exposes a configurable embedding pipeline via the `embedding` request block (and the corresponding CLI flags). Key options:

- `backend`: `fastembed` (default) loads ONNX models from the FastEmbed distribution, while `candle` bootstraps CPU-only sentence-transformers via Candle. Candle honours optional `batchSize` overrides and inherits model-specific requirements (download the model weights before running in offline environments).
- `model`: Case-sensitive identifier understood by the chosen backend (for example `Xenova/all-MiniLM-L6-v2`, `BAAI/bge-base-en-v1.5`, or the Candle variants listed in `embedding.rs`).
- `enabled`: Toggle embeddings entirely—lexical search continues to work when set to `false`.
- `chunkSizeTokens`, `chunkOverlapTokens`, and `batchSize`: Tune tokenizer slice length, overlap, and embedding batch size. Quantized FastEmbed models ignore batch size, while Candle honours whichever value is provided.

Embedders are cached per `(backend, model, batchSize)` tuple, so watches, search, and bundle operations reuse the warmed runner without paying model start-up costs.

When embeddings are written the ingest pass also attempts to persist an ANN index (`.ann/<basename>.hnsw.{graph,data}` plus a mapping file). Search mode opens that index lazily, falling back to brute-force scoring if the files are missing or corrupted; a warning is emitted so operators can investigate. The active basename and mapping filename are mirrored into the SQLite `meta` table (`embedding_ann_basename`, `embedding_ann_mapping`) for observability and cleanup.

## MCP Client Configuration Examples

The snippets below show how to wire this server into popular MCP clients. Adjust paths and logging preferences for your machine.

### Codex CLI (`~/.config/codex/config.toml`)

```toml
[mcp_servers.index_mcp]
command = "/path/to/index-mcp/start.sh"
# Optionally pass a mode (`production` by default) or extra flags.
# args = ["production"]
startup_timeout_sec = 30
tool_timeout_sec = 300

[mcp_servers.index_mcp.env]
INDEX_MCP_MODE = "production"          # or "development"
INDEX_MCP_LOG_LEVEL = "info"
INDEX_MCP_LOG_DIR = "/path/to/index-mcp/logs"
INDEX_MCP_LOG_CONSOLE = "true"
INDEX_MCP_BUDGET_TOKENS = "3000"
INDEX_MCP_RUNTIME = "rust"              # switch to "node" for the legacy server
RUST_LOG = "warn"
```

### Claude Code (`~/Library/Application Support/Claude/code/config.json` on macOS)

```json
{
  "mcpServers": {
    "index_mcp": {
      "command": "/path/to/index-mcp/start.sh",
      "env": {
        "INDEX_MCP_MODE": "production",
        "INDEX_MCP_LOG_LEVEL": "info",
        "INDEX_MCP_LOG_DIR": "/path/to/index-mcp/logs",
        "INDEX_MCP_LOG_CONSOLE": "true",
        "INDEX_MCP_BUDGET_TOKENS": "3000",
        "INDEX_MCP_RUNTIME": "rust",
        "RUST_LOG": "warn"
      },
      "timeout": {
        "startup": 30,
        "tool": 300
      }
    }
  }
}
```

Claude Code reads this JSON when the VS Code extension starts. Restart the extension (or VS Code) after editing the file so the new server registration takes effect.

## Watch Mode

Enable watch mode either via Cargo directly or with the helper script:

```bash
cargo run -p index-mcp-server --release -- --watch --watch-debounce=250
./start.sh dev                             # Debug build with watcher defaults
./start.sh production --watch --watch-no-initial
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

## Hybrid Search, Context Budget & Hotness Tracking

`semantic_search` now returns richer matches: every row includes a `source` flag (embedding vs lexical), a normalised `confidence`, and optional chunk metadata (`summary`, `symbol`, `identifier`, `sourceType`, `metadata`). Identifier-style prompts promote up to three high-confidence lexical hits before semantic results, and the response captures end-to-end timings via `diagnostics` (`embedding_latency_ms`, `lexical_latency_ms`, `total_latency_ms`, evaluated chunk counts, and the active model/backend).

`context_bundle` accepts an optional natural-language `query` that re-ranks snippets via the same embedding backend used for ingest. Bundles expose similarity scores, symbol metadata, and diagnostics (query echo, model/backend, latency, similarity range) so downstream prompts can weight each excerpt appropriately. Raw embedding vectors are stripped from the final payload, but the per-snippet metadata remains available for agents that need richer citations.

`ingest_codebase` persists the additional metadata (summary, symbol, identifier, language, graph metadata) to `file_chunks` and records `embedding_backend`, `embedding_dimension`, `embedding_quantized`, and `embedding_latency_ms` in both the `IngestResponse` and the backing SQLite `meta` table. Existing databases auto-migrate—missing columns are added on demand, and older indices continue to work until a fresh ingest backfills the new fields. When overriding `databaseName`, always supply a filename (for example `".mcp-index.sqlite"` or `"index-alt.sqlite"`); directory-like values such as `"."` cannot be opened by SQLite and will fail the ingest.

Context bundles still respect the `INDEX_MCP_BUDGET_TOKENS` environment variable (default: 3000 tokens). Responses prioritise focus definitions, append nearby lines, and truncate intelligently with explicit notices when content is trimmed. Each served chunk increments a `hits` counter which feeds auto-eviction heuristics during ingest.

To cap database size during ingest:

```json
{
  "root": ".",
  "autoEvict": true,
  "maxDatabaseSizeBytes": 150000000
}
```

Pass the payload above to the `ingest_codebase` tool (for example via the MCP client you are integrating with). The server evicts the least-used rows until the size target is met.

## Recommended Agent Workflow

- **Prime the index** at the start of every session: run `ingest_codebase { "root": "." }` or launch the server with `--watch`. Respect `.gitignore`, skip artifacts larger than 8 MiB, and configure `autoEvict`/`maxDatabaseSizeBytes` before the database grows out of control. If you need a custom SQLite location, pass `databaseName` as a filename (not a directory path) so the ingestor can create the file safely.
- **Check freshness before reasoning** by calling `index_status`. If `isStale` is true or HEAD moved, re-run ingest before answering questions.
- **Brief yourself on recent commits** with `repository_timeline` (and `repository_timeline_entry` when you need detailed diffs) so plans reflect the latest changes.
- **Assemble payloads with `code_lookup`**: start with `query="..."` to scope results, then request `file="..."` plus optional `symbol` bundles for the snippets you intend to cite. Bundle mode enforces this contract—include `file` (or a `query` that resolves to a file) and provide `symbol` as an object such as `{ "name": "perform_ingest" }` rather than a bare string so the request passes schema validation.
- **Deliver targeted context** using `context_bundle` with `budgetTokens` (or `INDEX_MCP_BUDGET_TOKENS`), include citations, and avoid dumping entire files into responses.
- **Refine without re-ingesting** by leaning on `semantic_search` or additional `context_bundle` calls for deeper dives.
- **Close the loop after edits**: re-run ingest (or keep watch mode active) and confirm with `index_status`/`info` so downstream tasks consume fresh data.

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
- **Embedding download issues** – Default runs rely on FastEmbed’s ONNX cache. Re-run ingest once connectivity returns, switch to Candle with pre-seeded weights, or disable embeddings via `{ "embedding": { "enabled": false } }`.
- **ANN index missing** – The `.ann` directory is regenerated on each ingest. If search logs warn about ANN load failures, remove the stale files and trigger a fresh ingest; the system will continue with brute-force scoring meanwhile.
- **Cold ingest latency** – Startup now preloads the quantized `Xenova/all-MiniLM-L6-v2` weights; the first ingest on a clean workspace drops to ~24s, and subsequent runs reuse the in-process cache so they finish in milliseconds.
- **SQLite locks** – Another process may hold the database. Retry after releasing the lock or configure a different database filename with `--watch-database`.
- **Watcher noise** – Increase debounce or enable `--watch-quiet` to reduce log output.

## Further Reading

- `docs/zero_shot_code_search.md` – deep dive into the hybrid embedding stack that powers `semantic_search`, `code_lookup`, and `context_bundle`.
- `rust-migration.md` – status tracker for the Rust rewrite (formerly `docs/rust-migration.md`).
- `rust-acceleration.md` – design notes and benchmarks for the native pipeline (formerly `docs/rust-acceleration.md`).
- `agents_repo.md` – repository-specific guidance for wiring the server into MCP-compatible clients.
- `agents_global.md` – Codex MCP best practices and global operating guidance.
- `IMPLEMENTATION_SUMMARY.md` – historical context for the token-budget and hotness tracking features.
