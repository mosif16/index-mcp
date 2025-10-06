# Rust Acceleration Notes

With the removal of the legacy `docs/` directory, performance tuning notes for the Rust migration now live directly in the repository root.

## Profiling Checklist

- Benchmark `ingest_codebase` with warm embedder cache enabled to capture steady-state performance.
- Capture flamegraphs for the walker pipeline (glob filtering, hashing, embedding) using `cargo flamegraph` or `tokio-console` sampling.
- Compare watcher-mode incremental ingests versus full rescan to ensure the hashing short-circuit remains effective.

## Observations

- Embedding throughput is now bound by the active backend: FastEmbed excels for quantized ONNX models while Candle favours CPU-friendly transformer stacks. The shared cache hides model spin-up regardless of backend choice.
- SQLite vacuuming dominates eviction passes on spinning disks. Consider deferring `VACUUM` to scheduled maintenance if latency spikes are observed.
- Switching the default to quantized `Xenova/all-MiniLM-L6-v2` shrinks the initial download and disables batching automatically to avoid incompatibilities; Candle deployments can opt into larger batch sizes when RAM permits.
- Streaming chunk batches through the embedder keeps peak RSS about ~2 GB (down from ~13 GB) while first ingest latency holds around 15.2 s on the reference workspace.
- Persisting ANN (`.ann`) files adds ~300 ms to ingest on medium repositories; search falls back to brute-force automatically if the graph load fails, so operators can delete the directory and re-run ingest without downtime.

## Next Steps

- [ ] Integrate structured timing into `ingest_codebase` responses (scan, chunk, embed, persist, ANN build) for easier regression detection.
- [ ] Profile Candle batch-size trade-offs and expose presets for low-memory environments.
- [ ] Measure ANN recall versus brute-force on representative corpora and surface the deltas in diagnostics.
