# Changelog

All notable changes to pi-rs are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com), and this project does not yet
adhere to semantic versioning — entries group development milestones.

## [0.1.0] - 2026-11-03

### Added

- Session tree surface: `/tree` selector over the persisted entry tree with
  branch navigation, optional LLM branch summaries, and the
  `branchSummary.skipPrompt` / `treeFilterMode` settings.
- Session replacement commands: `/fork` (user-message selector), `/clone`,
  `/import` (JSONL confirm-and-copy), `/export` (branch JSONL or rendered
  HTML), `/resume` selector with delete/rename, and `/name` display names.
- Model configuration: `models.json` object format with per-provider
  `apiKey`/`headers` (`!command` and `$VAR` resolution), `/provider` wizard,
  `/scoped-models` cycling scope, and `/model` selector overlays.
- Core ports: layered `settings.json` (global + project), keybindings.json
  overrides, user themes, project trust store, passthrough bash with live
  output preview, external editor (ctrl+g), compaction and auto-retry.

## [0.0.x] - 2026-10-14

### Added

- First runnable port: streaming TUI chat (ratatui), tool execution loop
  (read/edit/write/bash/grep/find), session persistence with tree-structured
  entries, `/help`, `/session`, `/thinking`, `/compact`, `/prompt`, `/tools`,
  and the MCP client with the startup project-trust gate.
