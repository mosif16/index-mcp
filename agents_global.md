# Codex MCP Best Practices

Transport
	•	Expose MCP servers over stdio whenever possible; fall back to a stdio↔HTTP/SSE bridge only when a remote must stay on HTTP.
	•	Keep stdout JSON-RPC only. Send logs, tracing, and diagnostics to stderr.

Framing
	•	Codex CLI expects newline-delimited JSON responses. Guard against accidental buffering or multiplexed stdout/stderr streams.
	•	Apply backpressure – bound queues, drop noisy events, and prefer structured logs.

Configuration
	•	Configure servers in `~/.codex/config.toml` under `[mcp_servers.<name>]` with `command`, optional `args`, and `env`.
	•	Tune `startup_timeout_sec` for slower binaries (Rust builds) and `tool_timeout_sec` for long-running ingests or history queries.

Security
	•	Inject secrets through environment variables or MCP headers; never bake them into binaries or config files.
	•	Require TLS for any remote MCP host that is not `localhost`.
	•	Validate `Origin` and `Host` headers inside HTTP/SSE bridges to stop rebinding attacks.

Streaming
	•	When proxying to SSE transports, preserve event order and IDs. Reconnect with exponential backoff.

Backward Compatibility
	•	Mirror remote tool names under a namespace (for example `docs.search`) so local clients avoid collisions.
	•	Keep behaviour consistent whether or not remotes are mounted.

Testing Checklist
	•	Validate stdout purity.
	•	Confirm cold-start latency stays within configured timeouts.
	•	Track ingest harness RSS; the streaming batch pipeline should stay near 2 GB versus the former 13 GB baseline.
	•	Exercise reconnection logic for SSE bridges.
	•	Verify `config.toml` changes are picked up after agent restarts.


# MCP Agent Guide (Global)

The guidance in this document applies to any MCP server integrated with the Codex CLI or other MCP-compatible runtimes. Use it as a baseline checklist when designing or operating agents across repositories.
