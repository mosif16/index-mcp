# Rust Acceleration Strategy for `index-mcp`

The existing ingestion pipeline does a considerable amount of synchronous filesystem and CPU-heavy work inside the Node.js event loop. While the current architecture is solid for small and medium sized repos, Rust can help in three areas where we consistently hit bottlenecks:

> **Note:** The Node runtime has now been removed from the repository. This document is preserved as a
> historical reference for the migration effort and highlights how the project arrived at the
> current Rust-only architecture.

> **Status (February 2026):** The Rust server (`crates/index-mcp-server`) now owns the entire ingestion, search, graph, and git timeline surface. The older Node pipeline has been removed. Native ingestion handles filesystem crawling, hashing, chunking, embeddings, TypeScript graph extraction, changed-path updates, auto-eviction, prompt routing, git timelines, and watch-mode orchestration. The remaining acceleration work focuses on incremental polish (token-count accuracy, AST optimization) rather than feature parity.

1. **Filesystem crawling + hashing** – now handled by Rust (multithreaded walker + hashing). Further work: refine adaptive batching and expose metrics to callers.
2. **Content chunking + tokenization** – currently implemented in Rust with byte/line metadata parity. Next milestone: optional exact-token counting via `tiktoken-rs` when clients request it.
3. **Graph extraction** – TypeScript/JavaScript extraction runs natively via `swc`. Future improvements: broaden language coverage (Rust/Go/Python) and share call graph metadata across remote proxies.

## Recommended Rust Architecture

1. **Build a `napi-rs` native module** (`crates/index_mcp_native`):
   - Export a `scan_repo` function that mirrors the current TypeScript `collectFilesToIndex` logic.
   - Use `ignore` crate to respect `.gitignore`, `.npmignore`, and `exclude` glob patterns.
   - Parallelize hashing + metadata collection with `rayon` (one worker per CPU core capped by configurable concurrency).
   - Return a vector of `{ path, size, mtime, hash, content? }` objects via serde serialization.

2. **Chunking and tokenization** *(Complete)*:
   - `analyze_file_content` now mirrors the JS chunker, returning byte/line metadata that the ingest pipeline feeds directly into embedding jobs.
   - The API accepts chunk size/overlap hints so clients can tune behaviour without round-tripping through JavaScript.
   - Future work: investigate integrating `tiktoken-rs` for exact token counts instead of the current heuristic.

3. **Graph extraction**:
   - For TypeScript/JavaScript, embed [`oxc`](https://github.com/oxc-project/oxc) or [`swc`](https://swc.rs/`) via Rust bindings to generate ASTs.
   - Extract symbol/edge data in Rust, emitting the existing schema ({ nodes, edges }) back to TypeScript.

4. **Packaging**:
   - Use [`@napi-rs/cli`](https://github.com/napi-rs/napi-rs/tree/main/cli) to produce prebuilt binaries for the major targets (darwin-x64/arm64, linux-x64/arm64, windows-x64).
   - Publish the crate as `@index-mcp/native` and add it as an optional dependency in `package.json`.
   - Gate the Node.js integration behind a runtime feature check so local development still works without the native module.

## Step-by-step Integration Plan

1. **Create the Rust crate skeleton**:
   ```bash
   cargo new crates/index_mcp_native --lib
   ```
   Add `napi`, `napi-derive`, `rayon`, `ignore`, `walkdir`, `serde`, and `tiktoken-rs` to `Cargo.toml`.

2. **Define shared TypeScript types** in `src/types/native.ts` that mirror the Rust structs. (Complete.)

3. **Loader utility** – `loadNativeModule` now selects the Rust implementation by default and falls back to legacy JS only when explicitly disabled. Expand error messaging with common troubleshooting hints (missing glibc, incompatible CPU).

4. **Node fallback (retired)** – the legacy TypeScript ingest path has been removed now that the Rust server has been battle-tested with large installations.

5. **Benchmark tracking** – maintain manual ingest benchmarks (`docs/benchmarks/`) for representative repos whenever we tweak the native pipeline. Include watch-mode deltas and auto-eviction timing.

6. **CI considerations** – GitHub Actions now builds the Rust server and native addon. Next: publish prebuilt artifacts for the server binary itself so downstream MCP hosts can download a ready-to-run executable.

## Expected Impact

| Area                     | Legacy JS throughput | Current Rust throughput | Notes |
|--------------------------|---------------------|-------------------------|-------|
| File scanning + hashing  | ~40–60 files/sec    | 220–250 files/sec       | 8-core M3 Pro, 1.2M file repo |
| Chunking/tokenization    | 8–10 MB/sec         | ~34 MB/sec              | Includes byte/line metadata for embeddings |
| Graph extraction         | 3–4k lines/sec      | ~11k lines/sec          | `swc`-based pipeline producing call/import graphs |
| Git timeline             | N/A                 | ~35 commits/sec         | `git log` parsing + diff summaries |

These improvements translate into faster initial ingest times (minutes → seconds on large repos) and lower CPU utilization during watch mode updates.

## Migration Tips

- Document the native build requirements (`cargo`, `rustup`, `cmake`) in the README.
- Provide troubleshooting docs for addon loading issues (missing glibc, incompatible CPU architecture, etc.).
- Make the release notes explicit that ingest now depends on the native addon so downstream clients can prepare their environments.
- Keep a follow-up list for: (a) porting the TypeScript graph extractor to a Rust AST pipeline (likely via `swc`/`oxc`), and (b) reintroducing parity checks (manual or automated) to assert `analyze_file_content` alignment with the legacy JavaScript chunker once a new validation strategy is defined.

By incrementally replacing the most CPU-bound parts of the pipeline with Rust, we can significantly improve throughput without sacrificing portability or developer ergonomics.
