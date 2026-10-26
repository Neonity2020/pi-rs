//! `find` tool — port of `pi/packages/coding-agent/src/core/tools/find.ts`.
//! Glob-style file-name search (the TS pi shells out to `fd`).

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::{param_str, param_str_or};

const MAX_RESULTS: usize = 500;

pub struct FindTool {
	cwd: PathBuf,
}

impl FindTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

/// Translate a glob (`*`, `?`) into an anchored regex.
fn glob_to_regex(glob: &str) -> String {
	let mut regex = String::from("^");
	let mut chars = glob.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'*' => regex.push_str(".*"),
			'?' => regex.push('.'),
			'.' => regex.push_str("\\."),
			'+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
				regex.push('\\');
				regex.push(c);
			}
			_ => regex.push(c),
		}
		let _ = chars.peek();
	}
	regex.push('$');
	regex
}

#[async_trait]
impl AgentTool for FindTool {
	fn name(&self) -> &str {
		"find"
	}

	fn description(&self) -> &str {
		"Find files by glob pattern matched against the path relative to the \
search root (default: cwd), e.g. `*.rs` or `src/**/*test*`. Results are capped \
at 500. Skips .git/node_modules/target."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"pattern": { "type": "string", "description": "Glob pattern (* and ?)" },
				"path": { "type": "string", "description": "Search root (default: cwd)" }
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
			Err(e) => return e,
		};
		let path = param_str_or(&params, "path", ".");

		let glob = pattern.trim_start_matches("./");
		// `src/**/*x` → match any relative path containing the trailing glob.
		let (prefix, tail) = match glob.split_once("**/") {
			Some((p, t)) => (Some(p.to_string()), t.to_string()),
			None => (None, glob.to_string()),
		};
		let regex = match Regex::new(&glob_to_regex(&tail)) {
			Ok(r) => r,
			Err(e) => return AgentToolResult::error(format!("invalid pattern: {e}")),
		};

		let root = super::resolve_path(&self.cwd, &path);
		let mut results: Vec<String> = Vec::new();
		let mut truncated = false;

		for entry in WalkDir::new(&root)
			.into_iter()
			.filter_entry(|e| {
				!(e.depth() > 0
					&& e.file_type().is_dir()
					&& matches!(
						e.file_name().to_string_lossy().as_ref(),
						".git" | "node_modules" | "target" | ".jj" | ".hg"
					))
			})
			.flatten()
		{
			if signal.is_cancelled() {
				return AgentToolResult::error("search aborted");
			}
			if !entry.file_type().is_file() {
				continue;
			}
			let rel = entry
				.path()
				.strip_prefix(&self.cwd)
				.unwrap_or(entry.path())
				.to_string_lossy()
				.into_owned();
			let name = rel.rsplit('/').next().unwrap_or(&rel);
			if !regex.is_match(name) {
				continue;
			}
			if let Some(prefix) = &prefix {
				if !rel.starts_with(prefix.as_str()) {
					continue;
				}
			}
			if results.len() >= MAX_RESULTS {
				truncated = true;
				break;
			}
			results.push(rel);
		}

		if results.is_empty() {
			return AgentToolResult::text(format!("no files matching `{pattern}`"));
		}
		let mut out = results.join("\n");
		if truncated {
			out.push_str(&format!("\n… (truncated at {MAX_RESULTS} results)"));
		}
		AgentToolResult::text(out)
	}
}
