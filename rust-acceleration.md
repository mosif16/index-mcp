# Rust Acceleration Notes

With the removal of the legacy `docs/` directory, performance tuning notes for the Rust migration now live directly in the repository root.

## Profiling Checklist

- Benchmark `ingest_codebase` with warm embedder cache enabled to capture steady-state performance.
- Capture flamegraphs for the walker pipeline (glob filtering, hashing, embedding) using `cargo flamegraph` or `tokio-console` sampling.
- Compare watcher-mode incremental ingests versus full rescan to ensure the hashing short-circuit remains effective.

## Observations

- Embedding throughput is now bound by `fastembed` model latency; the global cache eliminates repeated model initialization costs.
- SQLite vacuuming dominates eviction passes on spinning disks. Consider deferring `VACUUM` to scheduled maintenance if latency spikes are observed.

## Next Steps

- [ ] Integrate structured timing into `ingest_codebase` responses (scan, chunk, embed, persist) for easier regression detection.
- [ ] Experiment with parallel chunk embedding once `fastembed` exposes an async-safe API.

