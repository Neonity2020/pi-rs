//! `write` tool — port of `pi/packages/coding-agent/src/core/tools/write.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::{param_str, resolve_path};

pub struct WriteTool {
	cwd: PathBuf,
}

impl WriteTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

#[async_trait]
impl AgentTool for WriteTool {
	fn name(&self) -> &str {
		"write"
	}

	fn description(&self) -> &str {
		"Write a file to the local filesystem, overwriting it if it exists. \
Parent directories are created as needed. Prefer `edit` for partial changes."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string" },
				"content": { "type": "string" }
			},
			"required": ["path", "content"]
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
		let content = match param_str(&params, "content") {
			Ok(c) => c,
			Err(e) => return *e,
		};
		let resolved = resolve_path(&self.cwd, &path);

		if let Some(parent) = resolved.parent()
			&& let Err(e) = tokio::fs::create_dir_all(parent).await
		{
			return AgentToolResult::error(format!("cannot create parent dir for {path}: {e}"));
		}
		match tokio::fs::write(&resolved, &content).await {
			Ok(()) => {
				let lines = content.lines().count();
				AgentToolResult::text(format!("wrote {path} ({lines} lines)"))
			}
			Err(e) => AgentToolResult::error(format!("cannot write {path}: {e}")),
		}
	}
}
