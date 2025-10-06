# Zero-Shot Code Search Integration

This document describes the embedding-backed retrieval layer that now powers `semantic_search`, `code_lookup`, and `context_bundle`. The goal is to expose the repo through a single hybrid search stack that understands natural language prompts, code symbols, documentation, and file metadata without pre-written rules.

## Model Strategy

- **Backends**: `embedding.backend` selects the embedding runtime. `fastembed` (default) loads ONNX weights such as `Xenova/all-MiniLM-L6-v2` and its quantized variants; `candle` boots CPU-only sentence-transformer pipelines built on Candle. Both backends report `embedding_backend`, `embedding_dimension`, and `embedding_quantized` in `IngestResponse` and the SQLite `meta` table.
- **Extensibility**: the ingest pipeline accepts per-run overrides (`model`, `batchSize`, chunk sizing). Any model exposed by the chosen backend can be used (for example FastEmbed’s `BAAI/bge-base-en-v1.5` or Candle’s `sentence-transformers/all-MiniLM-L12-v2`). Candle honours batch-size hints, while quantized FastEmbed variants fall back to single-item embedding.
- **Caching**: embedder instances remain in-process via the shared cache from `ingest` and are reused by search and bundle flows. Query embeddings are created lazily, reusing the same cache entry.
- **ANN acceleration**: successful ingests produce a `.ann/<basename>.hnsw.{graph,data}` pair plus an `.ids` mapping file. These files are keyed by model/backend so multiple indices can coexist; metadata entries (`embedding_ann_basename`, `embedding_ann_mapping`) make the active pair discoverable at query time.

## Data Flow

```
┌────────┐   chunk + metadata   ┌──────────────┐   vector store
│ walker │ ───────────────────▶ │ ingest.rs    │ ───────────────▶ SQLite `file_chunks`
└────────┘                       │  • chunking  │   • content
                                │  • graph map │   • summary / symbol / identifier
                                │  • embed     │   • source_type / language / metadata JSON
                                └──────────────┘   • embedding BLOB + model tag
```

During hybrid search:

```
query ─▶ lexical sieve ─▶ embedding search ─▶ de-dupe + rank ─▶ response
                  │                        │
                  └───────────── timings & diag ──────────────┘
```

Context bundles reuse the same embeddings and run a second semantic pass to assemble high-signal snippets around the caller’s focus.

## Storage Schema Highlights

New columns on `file_chunks`:

| Column        | Description                                                       |
|---------------|-------------------------------------------------------------------|
| `summary`     | First non-empty line in the chunk (quick glance text)             |
| `symbol`      | Primary graph node covering the chunk (function / class / etc.)   |
| `identifier`  | Stable graph node id                                              |
| `source_type` | Heuristic label (`code`, `content`, …)                            |
| `language`    | Language detected at ingest time                                  |
| `metadata`    | JSON payload combining graph metadata + range offsets             |
| `embedding`   | Dense vector (f32) stored as BLOB                                 |

Meta table additions record `embedding_model`, `embedding_backend`, `embedding_dimension`, and `embedding_quantized` for downstream reporting.

## Retrieval Strategy

### Embedded search (default for `semantic_search`)
1. Load the query embedding through the configured backend (FastEmbed or Candle) using the shared cache.
2. If an ANN index is present, query the HNSW graph (with configurable `k`/`ef`) and project neighbours back to chunk ids; otherwise fall back to brute-force cosine scoring.
3. Normalize scores to `[0, 1]` and merge diagnostics covering backend, model, latencies, evaluated chunk counts, and whether ANN succeeded.
4. Return `SemanticSearchMatch` with confidence, summary, symbol metadata, and a `SearchSource` tag (`embedding` or `lexical`).

### Lexical infusion
- Identifier-style prompts (no whitespace, short tokens, or path-like strings) trigger a targeted `LIKE` sweep ordered by usage.
- Lexical hits receive confidence `1.0` and precede semantic matches while still respecting dedupe by chunk id.
- When embedding results are absent, lexical matches become the fallback path.

### Context bundles
- Optional `query` parameter embeds natural language or task prompts.
- Snippets carry similarity scores and are re-ranked before trimming to the token budget.
- Bundle diagnostics report which model/backends were used, optional latency, and similarity range to help tune downstream prompting.
- Graph-linked neighbors are expanded automatically: when a focused symbol references other files the bundle appends ranked snippets from those targets, with edge metadata attached so agents can cite the cross-file hop without extra lookups.

## Diagnostics Surface

- **Semantic search**: `diagnostics` returns `backend`, `model`, `quantized`, per-phase latencies, and chunk counts.
- **Context bundle**: diagnostics include the query (if provided), embedding model/backend, embedding latency, and similarity min/max for included snippets.
- **Ingest**: response now includes backend, dimension, latency, and quantized flags that feed into the search/bundle layers.

## Hybrid Ranking Summary

1. Canonical lexical matches seeded (identifier slots capped at 3 by default).
2. Embedding matches sorted by cosine similarity.
3. Dedupe by `(file, chunk_index)` while preserving lexical priority.
4. Confidence value supplied with each `SemanticSearchMatch` for consumer-side thresholding.
5. Suggested tool payloads still emitted to guide follow-up actions.

## Schema Back-Compatibility

The SQLite schema self-migrates: missing columns are added via `ALTER TABLE file_chunks ADD COLUMN ...`. Older indices remain compatible; new diagnostics simply show `None` until a fresh ingest populates metadata.

## Configuration & Tuning

- `ingest_codebase.embedding` block controls model choice, chunk sizes, and batching. Disable embeddings entirely by setting `enabled=false`; the tooling automatically falls back to lexical search.
- Query semantics inside `context_bundle` are optional and opt-in: omitting `query` leaves the bundle behavior unchanged.
- Meta keys (`embedding_backend`, `embedding_dimension`, etc.) are exposed via `index_status` for observability.

## Future Work

- Evaluate GPU-enabled Candle builds or optional GGUF runners for heavier models.
- Persist ANN health metrics (build durations, neighbour recall sampling) alongside ingest reports for easier regression tracking.
