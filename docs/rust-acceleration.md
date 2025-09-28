# Rust Acceleration Strategy for `index-mcp`

The existing ingestion pipeline does a considerable amount of synchronous filesystem and CPU-heavy work inside the Node.js event loop. While the current architecture is solid for small and medium sized repos, Rust can help in three areas where we consistently hit bottlenecks:

> **Status (September 2025):** The native module (`crates/index_mcp_native`) now ships with a multithreaded filesystem crawler and a chunk-analysis API that mirrors the JavaScript tokenizer. TypeScript graph extraction remains in the JS fallback while we evaluate a Rust-based AST pipeline.

1. **Filesystem crawling + hashing** – walking the tree, parsing `.gitignore` rules, and hashing every file happens serially today. The Node.js `fast-glob` + streaming hash combo is reliable, but we are limited to a single thread.
2. **Content chunking + tokenization** – chunking and token counting are implemented in TypeScript and rely on JavaScript strings. This becomes expensive for large files even before we call into the embedding model.
3. **Graph extraction** – parsing syntax trees (especially for TypeScript/JavaScript) can consume a lot of CPU and memory.

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

2. **Define shared TypeScript types** in `src/types/native.ts` that mirror the Rust structs. These types will become the contract for the bridge layer.

3. **Write a small loader utility**:
   - Implement `loadNativeModule()` that attempts to `import('@index-mcp/native')`.
   - Provide descriptive error messages when the native addon is missing or fails to initialize and abort ingestion when loading fails.

4. **Refactor `ingest.ts`** to:
   - Require the Rust `scan_repo` output and fail fast when the addon is unavailable.
   - Translate Rust results into the existing SQLite insertion pipeline without touching the downstream code.

5. **Add benchmarks and regression tests**:
   - Use `tests/benchmarks/ingest-benchmark.ts` to compare JavaScript vs. Rust ingestion on representative repo snapshots.
   - Track metrics (duration, memory usage, CPU usage) to ensure we do not introduce regressions.

6. **CI considerations**:
   - Extend the GitHub Actions workflow to build/publish the native addon artifacts on release tags.
   - Run `cargo fmt`/`cargo clippy`/`cargo test` alongside the existing `npm` tasks.

## Expected Impact

| Area                     | Current JavaScript throughput | Target Rust throughput |
|--------------------------|-------------------------------|------------------------|
| File scanning + hashing  | ~40–60 files/sec on medium repos | 200+ files/sec with multithreading |
| Chunking/tokenization    | 8–10 MB/sec                   | 30+ MB/sec leveraging native tokenization |
| Graph extraction         | 3–4k lines/sec                | 10k+ lines/sec via native AST tooling |

These improvements translate into faster initial ingest times (minutes → seconds on large repos) and lower CPU utilization during watch mode updates.

## Migration Tips

- Document the native build requirements (`cargo`, `rustup`, `cmake`) in the README.
- Provide troubleshooting docs for addon loading issues (missing glibc, incompatible CPU architecture, etc.).
- Make the release notes explicit that ingest now depends on the native addon so downstream clients can prepare their environments.
- Keep a follow-up list for: (a) porting the TypeScript graph extractor to a Rust AST pipeline (likely via `swc`/`oxc`), and (b) adding regression tests that assert `analyze_file_content` parity with the legacy JavaScript chunker.

By incrementally replacing the most CPU-bound parts of the pipeline with Rust, we can significantly improve throughput without sacrificing portability or developer ergonomics.
