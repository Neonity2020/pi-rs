# AGENTS.md

pi-rs is a Rust workspace that reimplements the pi coding agent philosophy
(a minimal agent harness). The governing rule: if we don't need it, it won't
be built.

## Layout

- `crates/llm` — provider layer. `Provider` trait, streaming SSE client,
  DeepSeek + OpenAI-compatible backends, model registry & pricing.
- `crates/agent` — agent loop. Requests → tool execution → `AgentEvent`
  stream. Knows nothing about terminals, sessions, or prompts.
- `crates/tui` — ratatui state and rendering. Owns no I/O; the coding layer
  feeds it `UiEvent`/keys and returns `Action`.
- `crates/coding` — the `pi` binary. CLI, session JSONL, system prompt
  assembly, the four tools (read/write/edit/bash), and the three run modes.

Dependency direction is strictly one-way: `coding -> agent -> llm`.
`tui` is a leaf used only by `coding`.

## Conventions

- Errors: `llm::Error` (thiserror) at the provider boundary; tool failures are
  `ToolResult::is_error`, never `Err`; `coding` uses anyhow.
- The message format (`llm::Message`) doubles as wire format and session
  record. Do not add a conversion layer.
- DeepSeek requires assistant `reasoning_content` to be echoed back verbatim
  whenever `tools` is present. Never strip it.
- No new dependencies without a strong reason (the current list is: tokio,
  reqwest, serde, serde_json, thiserror, tokio-stream, ratatui, crossterm,
  unicode-width, anyhow).
- Keep the base system prompt under ~400 tokens.

## Commands

- `cargo test` — unit tests + e2e tests (e2e spins a local mock SSE server
  and runs the real binary; no API key needed)
- `cargo clippy --all-targets` — must stay clean
- `cargo fmt`
- Run: `cargo run -p pi-coding -- -p "prompt"` (needs `DEEPSEEK_API_KEY`)
