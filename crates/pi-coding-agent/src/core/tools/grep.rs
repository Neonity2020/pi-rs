//! `grep` tool — port of `pi/packages/coding-agent/src/core/tools/grep.ts`.
//! pi shells out to a managed ripgrep; pi-rs walks with `walkdir` + `regex`
//! until the rg backend is wired.

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::{param_str, param_str_or};

const MAX_MATCHES: usize = 200;

pub struct GrepTool {
	cwd: PathBuf,
}

impl GrepTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

fn skip_dir(name: &str) -> bool {
	name == ".git" || name == "node_modules" || name == "target" || name == ".jj" || name == ".hg"
}

#[async_trait]
impl AgentTool for GrepTool {
	fn name(&self) -> &str {
		"grep"
	}

	fn description(&self) -> &str {
		"Search file contents with a regular expression. Returns `path:line:text` \
matches, capped at 200 hits. Searches recursively under `path` (default: cwd), \
skipping .git/node_modules/target."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"pattern": { "type": "string", "description": "Rust regex" },
				"path": { "type": "string", "description": "File or directory to search (default: cwd)" }
			},
			"required": ["pattern"]
		})
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let pattern = match param_str(&params, "pattern") {
			Ok(p) => p,
			Err(e) => return *e,
		};
		let path = param_str_or(&params, "path", ".");

		let regex = match Regex::new(&pattern) {
			Ok(r) => r,
			Err(e) => return AgentToolResult::error(format!("invalid regex: {e}")),
		};

		let root = super::resolve_path(&self.cwd, &path);
		if root.is_file() {
			let text = match std::fs::read_to_string(&root) {
				Ok(t) => t,
				Err(e) => return AgentToolResult::error(format!("cannot read {path}: {e}")),
			};
			let mut out = String::new();
			for (index, line) in text.lines().enumerate() {
				if regex.is_match(line) {
					out.push_str(&format!("{path}:{}:{}\n", index + 1, line));
				}
			}
			return if out.is_empty() {
				AgentToolResult::text("no matches")
			} else {
				AgentToolResult::text(out)
			};
		}

		let mut matches_out: Vec<String> = Vec::new();
		let mut truncated = false;
		for entry in WalkDir::new(&root)
			.into_iter()
			.filter_entry(|e| {
				!(e.depth() > 0
					&& e.file_type().is_dir()
					&& skip_dir(e.file_name().to_string_lossy().as_ref()))
			})
			.flatten()
		{
			if signal.is_cancelled() {
				return AgentToolResult::error("search aborted");
			}
			if !entry.file_type().is_file() {
				continue;
			}
			// Skip obvious binaries cheaply by extension.
			let ext = entry
				.path()
				.extension()
				.and_then(|e| e.to_str())
				.unwrap_or("");
			if matches!(
				ext,
				"png" | "jpg" | "jpeg" | "gif" | "pdf" | "zip" | "gz" | "lock"
			) {
				continue;
			}
			let Ok(text) = std::fs::read_to_string(entry.path()) else {
				continue;
			};
			let rel = entry
				.path()
				.strip_prefix(&self.cwd)
				.unwrap_or(entry.path())
				.to_string_lossy()
				.into_owned();
			for (index, line) in text.lines().enumerate() {
				if regex.is_match(line) {
					if matches_out.len() >= MAX_MATCHES {
						truncated = true;
						break;
					}
					matches_out.push(format!("{rel}:{}:{}", index + 1, line.trim_end()));
				}
			}
			if truncated {
				break;
			}
		}

		if matches_out.is_empty() {
			return AgentToolResult::text(format!("no matches for /{pattern}/"));
		}
		let mut out = matches_out.join("\n");
		if truncated {
			out.push_str(&format!("\n… (truncated at {MAX_MATCHES} matches)"));
		}
		AgentToolResult::text(out)
	}
}
