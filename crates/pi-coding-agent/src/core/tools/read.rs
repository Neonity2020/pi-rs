//! `read` tool — port of `pi/packages/coding-agent/src/core/tools/read.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::{param_str, param_u64_or, resolve_path};

const MAX_LINES: u64 = 2000;
const MAX_LINE_WIDTH: usize = 2000;

pub struct ReadTool {
	cwd: PathBuf,
}

impl ReadTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

#[async_trait]
impl AgentTool for ReadTool {
	fn name(&self) -> &str {
		"read"
	}

	fn description(&self) -> &str {
		"Read a text file from the local filesystem. Output is prefixed with \
line numbers (`cat -n` style). Supports optional offset/limit to read ranges \
of large files. Prefer this over bash cat."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string", "description": "File path (absolute or relative to cwd)" },
				"offset": { "type": "number", "description": "1-based line to start from" },
				"limit": { "type": "number", "description": "Maximum number of lines to read" }
			},
			"required": ["path"]
		})
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		_signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let path = match param_str(&params, "path") {
			Ok(p) => p,
			Err(e) => return *e,
		};
		let offset = param_u64_or(&params, "offset", 1).max(1);
		let limit = param_u64_or(&params, "limit", MAX_LINES).min(MAX_LINES);
		let resolved = resolve_path(&self.cwd, &path);

		match tokio::fs::read_to_string(&resolved).await {
			Ok(content) => {
				let mut out = String::new();
				let mut count = 0u64;
				for (index, line) in content.lines().enumerate() {
					let line_no = index as u64 + 1;
					if line_no < offset {
						continue;
					}
					if count >= limit {
						out.push_str(&format!(
							"... (truncated at line {line_no}, use offset to continue)\n"
						));
						break;
					}
					let line = if line.chars().count() > MAX_LINE_WIDTH {
						format!("{}…", line.chars().take(MAX_LINE_WIDTH).collect::<String>())
					} else {
						line.to_string()
					};
					out.push_str(&format!("{line_no:>6}\t{line}\n"));
					count += 1;
				}
				if out.is_empty() {
					out = "(empty file)\n".into();
				}
				AgentToolResult::text(out)
			}
			Err(e) => AgentToolResult::error(format!("cannot read {path}: {e}")),
		}
	}
}
