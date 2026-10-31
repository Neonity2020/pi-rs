//! System prompt builder — port of
//! `pi/packages/coding-agent/src/core/system-prompt.ts`.
//!
//! Includes global and ancestor project instruction files; other resources and
//! per-tool prompt snippets are still being ported.

use std::path::Path;

use pi_agent::AgentTool;

#[derive(Clone)]
pub struct SystemPromptOptions {
	pub context_files: bool,
	pub custom: Option<String>,
	pub append: Vec<String>,
}

impl Default for SystemPromptOptions {
	fn default() -> Self {
		Self {
			context_files: true,
			custom: None,
			append: Vec::new(),
		}
	}
}

pub fn build_system_prompt(
	cwd: &Path,
	tools: &[std::sync::Arc<dyn AgentTool>],
	context_files: bool,
) -> String {
	build_system_prompt_with_options(
		cwd,
		tools,
		&SystemPromptOptions {
			context_files,
			..Default::default()
		},
	)
}

pub fn build_system_prompt_with_options(
	cwd: &Path,
	tools: &[std::sync::Arc<dyn AgentTool>],
	options: &SystemPromptOptions,
) -> String {
	let tool_lines: Vec<String> = tools
		.iter()
		.map(|t| format!("- {}: {}", t.name(), first_line(t.description())))
		.collect();

	let mut prompt = format!(
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
	);
	if let Some(custom) = options.custom.as_deref().filter(|text| !text.is_empty()) {
		prompt = custom.to_string();
	}
	for text in &options.append {
		if !text.is_empty() {
			prompt.push_str("\n\n");
			prompt.push_str(text);
		}
	}
	if options.context_files {
		let files = load_project_context(cwd, &crate::config::agent_dir());
		if !files.is_empty() {
			prompt
				.push_str("\n\n# Project context\n\nProject-specific instructions and guidelines:");
			for (path, content) in files {
				prompt.push_str(&format!(
					"\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
					path.display(),
					content
				));
			}
		}
	}
	if options
		.custom
		.as_deref()
		.is_some_and(|text| !text.is_empty())
	{
		prompt.push_str(&format!("\n\n<cwd>\n{}\n</cwd>", cwd.display()));
	}
	prompt
}

pub fn resolve_prompt_input(input: &str) -> String {
	if input.is_empty() {
		return String::new();
	}
	if Path::new(input).exists() {
		match std::fs::read_to_string(input) {
			Ok(text) => return text.trim_start_matches('\u{feff}').to_string(),
			Err(error) => eprintln!("Warning: Could not read system prompt file {input}: {error}"),
		}
	}
	input.to_string()
}

/// Load the first matching file from the global agent directory and each
/// ancestor, in increasing scope order. Per-directory precedence matches pi.
fn load_project_context(cwd: &Path, agent_dir: &Path) -> Vec<(std::path::PathBuf, String)> {
	fn from_dir(dir: &Path) -> Option<(std::path::PathBuf, String)> {
		for name in [
			"AGENTS.override.md",
			"AGENTS.md",
			"AGENTS.MD",
			"CLAUDE.md",
			"CLAUDE.MD",
		] {
			let path = dir.join(name);
			if !path.is_file() {
				continue;
			}
			match std::fs::read_to_string(&path) {
				Ok(content) => {
					return Some((path, content.trim_start_matches('\u{feff}').to_owned()));
				}
				Err(error) => eprintln!("Warning: Could not read {}: {error}", path.display()),
			}
		}
		None
	}
	let global = from_dir(agent_dir);
	let mut ancestors: Vec<_> = cwd.ancestors().filter_map(from_dir).collect();
	ancestors.reverse();
	if let Some(global) = global {
		ancestors.retain(|(path, _)| *path != global.0);
		ancestors.insert(0, global);
	}
	ancestors
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

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn custom_prompt_and_append_resolve_files_and_keep_context_order() {
		let root = std::env::temp_dir().join(format!("pi-prompt-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		std::fs::write(root.join("AGENTS.md"), "PROJECT_CONTEXT").unwrap();
		let file = root.join("prompt.txt");
		std::fs::write(&file, "\u{feff}FILE_PROMPT").unwrap();
		assert_eq!(resolve_prompt_input(file.to_str().unwrap()), "FILE_PROMPT");
		assert_eq!(resolve_prompt_input("literal text"), "literal text");
		let options = SystemPromptOptions {
			context_files: true,
			custom: Some("CUSTOM".into()),
			append: vec!["FIRST".into(), "SECOND".into()],
		};
		let prompt = build_system_prompt_with_options(&root, &[], &options);
		assert!(prompt.starts_with("CUSTOM\n\nFIRST\n\nSECOND"));
		assert!(prompt.contains("PROJECT_CONTEXT"));
		assert!(prompt.contains(&root.display().to_string()));
		assert!(!prompt.contains("# Tools"));
		std::fs::remove_dir_all(root).unwrap();
	}
	#[test]
	fn context_precedence_order_and_bom() {
		let root = std::env::temp_dir().join(format!(
			"pi-context-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let project = root.join("project");
		let nested = project.join("nested");
		let global = root.join("global");
		std::fs::create_dir_all(&nested).unwrap();
		std::fs::create_dir_all(&global).unwrap();
		std::fs::write(global.join("AGENTS.md"), "global").unwrap();
		std::fs::write(project.join("CLAUDE.md"), "ignored").unwrap();
		std::fs::write(project.join("AGENTS.md"), "\u{feff}project").unwrap();
		std::fs::write(nested.join("AGENTS.override.md"), "nested").unwrap();
		let files = load_project_context(&nested, &global);
		let relevant: Vec<_> = files
			.iter()
			.filter(|(p, _)| p.starts_with(&root))
			.map(|(_, c)| c.as_str())
			.collect();
		assert_eq!(relevant, ["global", "project", "nested"]);
		std::fs::remove_dir_all(root).unwrap();
	}
}
