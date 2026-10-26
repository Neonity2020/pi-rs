//! `edit` tool — port of `pi/packages/coding-agent/src/core/tools/edit.ts`.
//! Exact-string replacement with uniqueness checks; the diff renderer from
//! `edit-diff.ts` lands with the interactive-mode polish pass.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::{param_bool_or, param_str, resolve_path};

pub struct EditTool {
	cwd: PathBuf,
}

impl EditTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

#[async_trait]
impl AgentTool for EditTool {
	fn name(&self) -> &str {
		"edit"
	}

	fn description(&self) -> &str {
		"Edit a file by exact string replacement. `old_string` must match the \
file content exactly and must be unique unless `replace_all` is true. Always \
`read` the file first."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string" },
				"old_string": { "type": "string" },
				"new_string": { "type": "string" },
				"replace_all": { "type": "boolean" }
			},
			"required": ["path", "old_string", "new_string"]
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
			Err(e) => return e,
		};
		let old_string = match param_str(&params, "old_string") {
			Ok(s) => s,
			Err(e) => return e,
		};
		let new_string = match param_str(&params, "new_string") {
			Ok(s) => s,
			Err(e) => return e,
		};
		let replace_all = param_bool_or(&params, "replace_all", false);

		if old_string == new_string {
			return AgentToolResult::error("old_string and new_string are identical");
		}

		let resolved = resolve_path(&self.cwd, &path);
		let content = match tokio::fs::read_to_string(&resolved).await {
			Ok(c) => c,
			Err(e) => return AgentToolResult::error(format!("cannot read {path}: {e}")),
		};

		let occurrences = content.matches(&old_string).count();
		if occurrences == 0 {
			return AgentToolResult::error(format!(
				"old_string not found in {path}. Read the file and check exact whitespace/indentation."
			));
		}
		if occurrences > 1 && !replace_all {
			return AgentToolResult::error(format!(
				"old_string occurs {occurrences} times in {path}; add surrounding context to make it unique or set replace_all"
			));
		}

		let updated = if replace_all {
			content.replace(&old_string, &new_string)
		} else {
			content.replacen(&old_string, &new_string, 1)
		};
		match tokio::fs::write(&resolved, &updated).await {
			Ok(()) => AgentToolResult::text(format!(
				"edited {path}: {} replacement(s)",
				if replace_all { occurrences } else { 1 }
			)),
			Err(e) => AgentToolResult::error(format!("cannot write {path}: {e}")),
		}
	}
}
