# Zero-Shot Code Search for `index-mcp`

This document describes the zero-shot, hybrid search stack added to the Rust `index-mcp` server. The goal is to let natural-language queries retrieve relevant code, documentation, and metadata without pre-defined keyword rules while preserving lexical precision when symbols are known.

## Model Selection and Embedding Strategy

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Embedding backend | [`fastembed`](https://github.com/huggingface/fastembed) ONNX runner | Stable Rust-native API, small runtime footprint, and support for quantized weights. |
| Default model | `Xenova/all-MiniLM-L6-v2` (configurable) | 384-dim text/code encoder that fits within <50 MB when quantized; balances recall and CPU cost. |
| Optional models | Any `fastembed` identifier (e.g., `BGESmallENV15Q`, `NomicEmbedTextV1.5`) | Users can trade off accuracy vs. footprint by passing `embedding.model` during ingest. |
| Quantization log | Every ingest logs model name, dimension, backend, and quantization flag to `tracing` once embeddings activate. |

Embeddings are computed for:

- Sliding window code chunks (default 256-token window with 32-token overlap).
- Leading docstrings and inline comments (captured in metadata and included in chunk text).
- Per-file metadata chunks (`chunk_index = -1`) summarising filenames and top-of-file docs.

Each chunk is persisted with:

- File path
- Chunk index
- Symbol identifier (heuristically inferred)
- Raw content and embedding vector
- Optional metadata JSON (source type + docstring snippet)

## Data Flow Overview

```
+----------------+      +---------------------+      +-------------------+
| ingest_codebase|----->| chunk builder       |----->| embedding engine  |
| (watch / manual)|     |  - chunk + metadata |      | (fastembed ONNX)  |
+----------------+      |  - symbol heuristics|      +-------------------+
        |                \                               |
        v                 \---> SQLite `file_chunks` <----+
+----------------+                    |                          
| code graph     |                    | metadata (symbol/doc)    
| extraction     |                    v                          
+----------------+              +--------------+                  
                                 | file summary |                  
                                 +--------------+                  
```

During ingest the server ensures legacy databases gain the new `symbol` and `metadata` columns, so existing users can upgrade without manual migrations.

## Retrieval Pipeline

1. **Query classification** – `classify_query` inspects the search text to detect natural language, symbol, file path, or code fragment intent.
2. **Adaptive search** – `code_lookup` search mode calls `adaptive_search`, which orchestrates:
   - Embedding similarity via cosine dot products for natural language or mixed queries.
   - Lexical fallbacks (symbol equality + `LIKE` scans) when identifiers are explicit or embeddings are unavailable.
3. **Result unification** – Semantic and lexical matches are merged, deduplicated by `(path, chunk_index)`, and ranked by confidence, preserving semantic hits before lexical fallbacks.
4. **Diagnostics** – Each response reports strategy, latency per phase, model metadata, and duplicate counts, surfaced through tool metadata.
5. **Context assembly** – `context_bundle` uses chunk metadata (symbols, docstrings, source type) to assemble semantically cohesive snippets, prioritising query-adjacent embeddings over naive adjacency.

## Embedding Schema

SQLite `file_chunks` now stores:

| Column | Description |
|--------|-------------|
| `symbol` | Optional identifier inferred from chunk content (first `fn/def/class` match). |
| `metadata` | JSON blob `{ "source": "code|filename", "docstring": "...", "symbol": "..." }`. |
| `embedding` | `BLOB` (little-endian `f32`) representing semantic vector. |

### Metadata Example

```json
{
  "source": "code",
  "symbol": "create_client",
  "docstring": "Create a Supabase client using cached secrets."
}
```

## Hybrid Search Strategies

- **Natural language** → embedding search ranks matches, lexical fallback only if semantic stage is empty.
- **Symbol or file path** → run both embedding and lexical phases; symbol equality ensures deterministic hits, embeddings recover related helpers.
- **Code fragments** → treat as hybrid (semantic for structural similarity, lexical for exact snippet recovery).
- **No embeddings stored** → adaptive search automatically skips the embedding phase and returns lexical matches, logging the fallback.

## Context Bundles

`context_bundle` leverages stored metadata to:

- Promote snippets whose embeddings align with the triggering query or symbol.
- Filter out synthetic filename chunks unless explicitly requested.
- Surface docstrings and comments alongside implementations for richer reasoning context.

### Diagnostics & Logging

- `SearchDiagnostics` includes strategy (`semanticOnly`, `lexicalOnly`, `hybrid`), latency per phase, embedding model info, and duplicate counts. The diagnostics are exposed via `code_lookup` metadata for downstream tools.
- Ingest logging emits a single `info!` entry when embeddings warm up, capturing model name, dimension, backend, and quantization flag.

## Configuration Tips

- Override the embedder via `ingest_codebase` → `{ "embedding": { "model": "BGESmallENV15Q" } }` for a 384-dim quantized encoder.
- Disable embeddings (`"enabled": false`) to force lexical mode (the adaptive search will honour the fallback path).
- Re-run `ingest_codebase` after edits so new symbols/docstrings get embedded and indexed.

With these changes, `index-mcp` supports zero-shot, multimodal search that combines fast lexical precision with semantic recall across code, docs, and metadata.
