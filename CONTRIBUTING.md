# Contributing to `index-mcp`

Thank you for caring about this server. Lean improvements compound quickly; the sections below keep changes predictable and production-ready.

## Core Expectations
- **Follow the mandated MCP workflow.** In development you can use the mock embedding backend (`embedding.backend="mock"`) to run integration tests without downloading models. Production binaries only ship FastEmbed (default) and Candle backends.
- **Stay deterministic.** Prefer pure functions, explicit inputs, and clear error paths. Map new behaviours to tests before you ship code.
- **Document the surface.** Update the relevant guide in `docs/` (implementation summary, zero-shot search, acceleration, etc.) when behaviour, defaults, or tooling changes.

## Getting Set Up
1. Install the Rust toolchain (stable) and ensure `cargo` is on your `PATH`.
2. Clone the repository and run an initial ingest in a scratch workspace to prime `.mcp-index.sqlite` if you plan to exercise the end-to-end flow.
3. Use the MCP workflow scripts (`ingest_codebase`, `semantic_search`) to understand existing behaviour—semantic search now covers bundle/lookup attachments without standalone tools.

## Required Checks
Run these for every contribution:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all --all-targets
```
Integration tests depend on the mock embedding backend and do not require external downloads. If any check cannot run locally, call it out clearly in your pull request.

## Making Changes
- **Feature work:** start with a test (unit or integration), implement the smallest surface area, update docs, then re-run the test suite.
- **Bug fixes:** add regression coverage, explain the failure mode in the commit or PR body, and document mitigation if it touches production config.
- **Performance tweaks:** capture measurements (before/after, methodology) and summarize them in `docs/rust-acceleration.md`.
- **Documentation-only updates:** keep prose concise, note sources when relevant, and link to supporting guides.

## Open Contribution Areas
- Expanding integration coverage (new edge cases for ingest/search/bundle/watch flows).
- Observability hooks (structured logging, metrics, diagnostics surfaced via MCP).
- Performance profiling and tuning (batching, ANN behaviour, auto-eviction heuristics).
- Language support in graph extraction and snippet post-processing.

## Pull Request Checklist
- [ ] Tests added/updated and all checksgreen.
- [ ] Documentation refreshed where behaviour changed.
- [ ] No secrets, credentials, or private data introduced.
- [ ] Behaviour verified against the mandated MCP workflow.
- [ ] Decision trade-offs captured in the PR description.

See `docs/mcp_best_practices.md` and `docs/rust-best-practices.md` for deeper guidance. Thank you for helping refine this server.
