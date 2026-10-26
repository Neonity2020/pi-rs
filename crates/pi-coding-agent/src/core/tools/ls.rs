//! `ls` tool — port of `pi/packages/coding-agent/src/core/tools/ls.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::{param_str_or, resolve_path};

pub struct LsTool {
	cwd: PathBuf,
}

impl LsTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

#[async_trait]
impl AgentTool for LsTool {
	fn name(&self) -> &str {
		"ls"
	}

	fn description(&self) -> &str {
		"List a directory's entries with type and size, one per line. \
Directories carry a trailing `/`."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string", "description": "Directory (default: cwd)" }
			},
			"required": []
		})
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		_signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let path = param_str_or(&params, "path", ".");
		let resolved = resolve_path(&self.cwd, &path);

		let entries = match tokio::fs::read_dir(&resolved).await {
			Ok(entries) => entries,
			Err(e) => return AgentToolResult::error(format!("cannot list {path}: {e}")),
		};

		let mut entries = entries;
		let mut rows: Vec<(String, bool, u64)> = Vec::new();
		while let Ok(Some(entry)) = entries.next_entry().await {
			let name = entry.file_name().to_string_lossy().into_owned();
			let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
			let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
			rows.push((name, is_dir, size));
		}
		rows.sort_by(|a, b| a.0.cmp(&b.0));

		if rows.is_empty() {
			return AgentToolResult::text(format!("{path} is empty"));
		}
		let mut out = String::new();
		for (name, is_dir, size) in rows {
			if is_dir {
				out.push_str(&format!("{name}/\n"));
			} else {
				out.push_str(&format!("{name}\t{size}B\n"));
			}
		}
		AgentToolResult::text(out)
	}
}
