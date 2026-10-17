# AGENTS.md

pi-rs is a Rust workspace that reimplements the Pi coding agent philosophy
(a minimal agent harness). The governing rule: if we don't need it, it won't
be built.

Repository: https://github.com/Neonity2020/pi-rs

## Layout

- `crates/llm` — provider layer. `Provider` trait, streaming SSE client,
  DeepSeek + OpenAI-compatible backends, model registry & pricing.
- `crates/agent` — agent loop. Requests → tool execution → `AgentEvent`
  stream, plus Pi-style context compaction (threshold and overflow). Knows
  nothing about terminals, sessions, or prompts.
- `crates/tui` — ratatui state and rendering. Owns no I/O; the coding layer
  feeds it `UiEvent`/keys and returns `Action`.
- `crates/coding` — the `pi-rs` binary. CLI, session JSONL, system prompt
  assembly, long-term memory files, the four tools (read/write/edit/bash),
  and the three run modes.
- `website/` — Astro landing page. Built outside Cargo, but it is a complete
  part of the project: its copy must describe pi-rs as it actually is. Do not
  invent features, providers, or claims, and do not grow the page with
  content unrelated to the project. Update it when user-facing behavior
  changes.
- `docs/` — maintainer docs: goals, architecture, plan, development. Keep
  `docs/architecture.md` in sync with structural changes.

Dependency direction is strictly one-way: `coding -> agent -> llm`.
`tui` is a leaf used only by `coding`.

## Conventions

- Errors: `llm::Error` (thiserror) at the provider boundary; tool failures are
  `ToolResult::is_error`, never `Err`; `coding` uses anyhow.
- The message format (`llm::Message`) doubles as wire format and session
  record. Do not add a conversion layer.
- DeepSeek requires assistant `reasoning_content` to be echoed back verbatim
  whenever `tools` is present. Never strip it.
- Compaction is a `type:"compaction"` session entry. The summary is replayed
  as a synthetic user message (`agent::summary_message`), so `Message` stays
  the only message type. Cut only at user/assistant messages, never between a
  tool call and its results; never rewrite retained `reasoning`. Summary
  requests count toward token/cost stats.
- Long-term memory is two visible Markdown files: `$PI_RS_HOME/MEMORY.md`
  (global) and `$PI_RS_HOME/memory/<cwd-slug>.md` (project). `coding::memory`
  appends a bounded recall section to the system prompt (about 1,200 estimated
  tokens; CJK-aware) and provides `/remember`, `/forget`, `/memory`. Writes go
  through the existing four tools, never a fifth tool. Do not auto-extract
  memory from conversations or repository content; only write when the user
  explicitly asks. Memory is context, not instructions, and must never contain
  secrets.
- Images are base64 `llm::Image` values on user messages (session format).
  Clipboard capture lives in `coding::clipboard` and shells out to the OS
  (osascript / wl-paste / xclip / powershell) — no image crate, no new
  dependency. The provider expands `images` into OpenAI `content` parts;
  text-only messages keep the plain string form.
- No new dependencies without a strong reason (the current list is: tokio,
  reqwest, serde, serde_json, thiserror, tokio-stream, ratatui, crossterm,
  unicode-width, anyhow, dotenvy).
- `pi-rs` loads `.env` at startup (dotenvy: the working directory or the nearest
  parent; real environment variables win). e2e runs the binary in a fresh
  temp CWD so a developer's `.env` cannot leak into tests.
- API keys resolve as `--api-key` > env (incl. `.env`) > `$PI_RS_HOME/auth.json`.
  Only `pi-rs login` / `/login` writes auth.json (verify with `GET /models` first,
  then atomic write, mode 0600). Tests must isolate `PI_RS_HOME`.
- Keep the base system prompt under ~400 tokens.
- TUI click targets are render-recorded line ranges (`thinking_hits`); the
  `coding` layer forwards mouse coordinates and `tui` still owns no I/O.

## Commands

- `cargo test` — unit tests + e2e tests (e2e spins a local mock SSE server
  and runs the real binary; no API key needed)
- `cargo clippy --all-targets` — must stay clean
- `cargo fmt`
- PTY smoke tests for the TUI: `python3 scripts/tui_smoke.py <home> target/debug/pi-rs`
  and `python3 scripts/tui_chat_smoke.py <home> target/debug/pi-rs`
- Website screenshot: `scripts/shot_website.sh` rebuilds `website/dist` and
  regenerates `website/screenshot.png` (needs Node + Chrome)
- Run: `cargo run -p pi-coding -- -p "prompt"` (needs `DEEPSEEK_API_KEY`)
