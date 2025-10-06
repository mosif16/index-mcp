# Agent Guidance Index

The previous `agents.md` contents now live in two focused guides:

- [`agents_global.md`](agents_global.md) – Global Codex MCP best practices that apply across repositories.
- [`agents_repo.md`](agents_repo.md) – index-mcp specific setup, tooling, and workflow guidance for this repository.
- [`codex-rust-info.md`](codex-rust-info.md) – Rust-focused MCP integration details, build tooling, and client notes.
- [`IMPLEMENTATION_SUMMARY.md`](IMPLEMENTATION_SUMMARY.md) – High-level overview of the Rust server architecture and tool surface.
- [`mcp_best_practices.md`](mcp_best_practices.md) – Authoritative guidance on MCP tool design, evaluation, and optimization.
- [`README.md`](README.md) – Project-level introduction, capabilities, and usage instructions.
- [`rust-acceleration.md`](rust-acceleration.md) – Performance profiling and optimization notes for Rust ingestion.
- [`rust-best-practices.md`](rust-best-practices.md) – Production readiness, security, and observability guidance for Rust MCP servers.

Update both documents together when workflows change so global expectations and repo details stay aligned.
## Required Local Testing

With the GitHub workflows removed, agents must run these checks before handing work back to the user:

- `cargo fmt --all -- --check"
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-targets`

If any command cannot be executed, explain why in the final response and highlight follow-up steps for the user.
## Binary packaging

When a user asks for a runnable binary or release artifact:

- Run `cargo build --release -p index-mcp-server` to refresh `target/release/index-mcp-server`.
- Point MCP configs at that binary (for example `command = "/path/to/target/release/index-mcp-server"`) unless the user prefers `start.sh`.
- Mention that GitHub Releases require manually uploading the binary or wiring an automation workflow; cargo does not publish binaries automatically.
- Remind the user to create the `logs/` directory if logging is enabled in their config.

Keep these notes aligned with `agents_repo.md` when workflows change.

