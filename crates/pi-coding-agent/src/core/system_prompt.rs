//! System prompt builder — port of
//! `pi/packages/coding-agent/src/core/system-prompt.ts`.
//!
//! Framework stage: identity, environment block, and tool list. Project
//! context files (AGENTS.md / CLAUDE.md / .pi/prompts) and per-tool prompt
//! snippets land with the extensions phase.

use std::path::Path;

use pi_agent::AgentTool;

pub fn build_system_prompt(cwd: &Path, tools: &[std::sync::Arc<dyn AgentTool>]) -> String {
	let tool_lines: Vec<String> = tools
		.iter()
		.map(|t| format!("- {}: {}", t.name(), first_line(t.description())))
		.collect();

	format!(
		"You are {app} {version}, a coding agent running in the user's terminal. \
You help with software engineering tasks: reading and editing code, running \
commands, and answering questions about the repository.

# Environment

- Working directory: {cwd}
- Platform: {platform}
- Date: {date}
- Non-interactive shell: sh -c

# Tools

You can use these tools. Read tool descriptions carefully before calling; \
prefer the dedicated file tools over bash for file operations.

{tools}

# Working style

- Explore before editing: read the relevant files first.
- Make minimal, focused edits; match the surrounding code style.
- Verify your work (build/test) when feasible.
- Never invent file contents; read them.

# Security

- Only operate within the working directory unless the user asks otherwise.
- Never run destructive commands (rm -rf, force push, ...) without explicit user request.",
		app = crate::config::APP_NAME,
		version = crate::config::VERSION,
		cwd = cwd.display(),
		platform = std::env::consts::OS,
		date = today(),
		tools = tool_lines.join("\n"),
	)
}

fn first_line(s: &str) -> String {
	s.lines().next().unwrap_or("").to_string()
}

fn today() -> String {
	let ms = pi_ai::types::now_ms();
	let secs = ms / 1000;
	let days = secs / 86400;
	let z = days as i64 + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let mth = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if mth <= 2 { y + 1 } else { y };
	format!("{y:04}-{mth:02}-{d:02}")
}
