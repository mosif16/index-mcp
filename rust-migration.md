# Rust Rewrite Status

The legacy `docs/` directory has been retired. Migration notes now live at the repository root so downstream agents and package managers no longer need to ship an extra documentation tree.

## Current Focus

- **Ingestion performance:** The Rust ingestor now caches `fastembed` models in-process, trimming repeat `ingest_codebase` runs from ~27s cold-starts down to single-digit millisecond refreshes once the cache is primed.
- **Parity validation:** Continue verifying graph extraction and chunk eviction parity with the TypeScript implementation. No regressions have been observed in the latest regression suite.
- **Watch mode:** `--watch` remains the primary mechanism for continuous ingestion; track notify back-offs while Linux inotify churn fixes are upstreamed.

## Open Tasks

- [ ] Expand the cached embedder registry to support per-model statistics (hit rate, warm time) for future auto-tuning.
- [ ] Document the new ingestion timeline metrics surfaced by `repository_timeline` when run against the Rust server.
- [ ] Audit consumer agents to ensure they respect the relocated documentation paths.

## Recent Changes

| Date | Change | Notes |
|------|--------|-------|
| 2025-10-02 | Embedder cache | Added a global `once_cell` cache to reuse `fastembed` models across ingests, minimizing cold-start overhead. |
| 2025-10-02 | Docs relocation | Removed `docs/` directory; root-level markdown files replace former paths referenced by the Node server. |
