# Codex MCP Context Notes

## Turn Context Lifecycle
- `TurnContext` captures per-turn state for the model, including the active `ModelClient`, working directory, persisted/base instructions, approval and sandbox policies, shell environment policy, configured tools, review mode flag, and any JSON schema for final output, so subsequent calls reuse a consistent environment until it is overridden.[^1]
- The submission loop responds to `Op::OverrideTurnContext` by cloning the previous context, calculating effective model family and reasoning settings, and materializing a fresh `TurnContext`; this becomes the new default for the session while preserving auth and provider wiring.[^2]
- Every new task clones the current context and compares its derived `EnvironmentContext` against the prior value; if anything (cwd, approval/sandbox, etc.) changes, a context message is recorded and pushed to listeners before the updated context is installed.[^3]

## Environment Context Emission
- `EnvironmentContext` normalizes sandbox state into portable fields (cwd, approval policy, sandbox mode, network access, writable roots, shell) derived from the active `SandboxPolicy`.[^4]
- `EnvironmentContext` implements `From<&TurnContext>` and serializes to a `<environment_context>…</environment_context>` XML payload; the result is wrapped in a `ResponseItem::Message`, making context changes appear as ordinary transcript entries.[^5]
- When a session starts, `build_initial_context` seeds the conversation history with user instructions (if any) plus an `EnvironmentContext` message so downstream clients can reconstruct the execution environment.[^6]
- `map_response_item_to_event_messages` converts those response items into `EventMsg::UserMessage` events; because updates are emitted whenever the context differs, MCP listeners see environment mutations in real time.[^7]
- The protocol parser classifies these XML-wrapped messages as `InputMessageKind::EnvironmentContext`, allowing clients to detect and treat them specially instead of rendering raw XML.[^8]

## MCP Surface for Context Control
- Session creation already accepts context knobs: `newConversation` parameters let callers set the model, working directory, approval policy, sandbox mode, optional config overrides, and alternate base instructions, plus flags for plan/apply-patch tools.[^9]
- During an active conversation, `sendUserTurn` requires explicit context arguments (`cwd`, `approvalPolicy`, `sandboxPolicy`, `model`, optional reasoning `effort`, and `summary`), so MCP clients can adjust these per turn without issuing a separate override call.[^10]
- Separately, the daemon exposes `Op::OverrideTurnContext`, letting higher-level flows (CLI or MCP) atomically change cwd, sandbox, model, or reasoning settings and propagate them to future tasks.[^2]

## Context Visibility & Persistence
- Turn context snapshots are persisted alongside responses as `RolloutItem::TurnContext` entries, meaning rollouts and resume flows retain the exact cwd, approval policy, sandbox settings, and model that were in effect for each turn.[^11]
- Rollout readers keep these items when reconstructing transcripts, so offline tooling (e.g., resume picker) can recover the same metadata a live MCP client would have seen.[^12]
- `TokenUsage` computes both absolute token usage and the percentage of the model’s effective context window remaining, factoring reasoning-token ejection and a configurable baseline; the UI consumes this to surface context window telemetry while tasks run.[^13]

[^1]: codex-rs/core/src/codex.rs:259
[^2]: codex-rs/core/src/codex.rs:1090
[^3]: codex-rs/core/src/codex.rs:1265
[^4]: codex-rs/core/src/environment_context.rs:15
[^5]: codex-rs/core/src/environment_context.rs:75; codex-rs/core/src/environment_context.rs:115
[^6]: codex-rs/core/src/codex.rs:732
[^7]: codex-rs/core/src/codex.rs:804; codex-rs/core/src/codex.rs:1282
[^8]: codex-rs/protocol/src/protocol.rs:730
[^9]: codex-rs/app-server-protocol/src/protocol.rs:194
[^10]: codex-rs/app-server-protocol/src/protocol.rs:535
[^11]: codex-rs/protocol/src/protocol.rs:937
[^12]: codex-rs/core/src/rollout/recorder.rs:214
[^13]: codex-rs/protocol/src/protocol.rs:615
