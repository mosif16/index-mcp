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

## /compact Command
- The TUI exposes `/compact` as a built-in slash command and dispatches it as `AppEvent::CodexOp(Op::Compact)` once selected or confirmed, so the UI never sends the literal text to the model.[^14]
- Core handles `Op::Compact` by trying to inject the summarization prompt into the active task; if injection fails it spawns `CompactTask` so the conversation is summarized as a background task.[^15]
- `CompactTask` streams the prompt defined in `compact/prompt.md`, gathers prior user messages, produces a bridge message with `history_bridge.md`, and replaces history with the summary output.[^16][^17]
- When a turn hits the provider token limit, the engine automatically calls the same inline compact routine to shrink history before continuing; a second failure surfaces an error to the user.[^18]
- The experimental MCP interface only exposes conversation/message RPCs (e.g., `sendUserMessage`, `sendUserTurn`) and utility endpoints; there is no request that maps to `Op::Compact`, and the MCP `InputItem` type only supports text/image payloads, so sending `/compact` via MCP just becomes another user message.[^19]
- Automatic compaction is only evaluated when the `ResponseEvent::Completed` we receive from the model carries token usage; on the Chat Completions transport used for MCP tool calls the stream adapter emits `token_usage: None`, so `token_limit_reached` never flips to `true` during those turns and the inline auto-compact path is skipped.[^20][^21][^22]
- To trip auto-compaction from MCP you would have to change that transport so `ResponseEvent::Completed` includes a populated `TokenUsage`—for example by switching the MCP path to the Responses API (which returns usage numbers) or by extending the streaming adapter to estimate usage before emitting the completion event. Without such a core change, an MCP client cannot make the engine auto-compact on its own.[^20][^21][^22]

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
[^14]: codex-rs/tui/src/slash_command.rs:38; codex-rs/tui/src/chatwidget.rs:1104
[^15]: codex-rs/protocol/src/protocol.rs:169; codex-rs/core/src/codex.rs:1399-1407
[^16]: codex-rs/core/src/tasks/compact.rs:12-26; codex-rs/core/src/codex/compact.rs:44-150
[^17]: codex-rs/core/templates/compact/prompt.md:1-5; codex-rs/core/templates/compact/history_bridge.md:1-7
[^18]: codex-rs/core/src/codex.rs:1801-1822
[^19]: codex-rs/docs/codex_mcp_interface.md:16-117; codex-rs/app-server-protocol/src/protocol.rs:55-210; codex-rs/protocol/src/protocol.rs:394-410
[^20]: codex-rs/core/src/codex.rs:1688-1705
[^21]: codex-rs/core/src/codex.rs:2104-2117
[^22]: codex-rs/core/src/chat_completions.rs:399-437
