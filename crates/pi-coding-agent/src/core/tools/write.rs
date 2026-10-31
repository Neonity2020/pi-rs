//! `write` tool — port of `pi/packages/coding-agent/src/core/tools/write.ts`.
//!
//! Behavior matches upstream: `path` + `content`, parent directories are
//! created as needed (mkdir -p), the file is overwritten when it exists.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdate};

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
		"Write content to a file. Creates the file if it doesn't exist, \
overwrites if it does. Automatically creates parent directories."
	}

	fn execution_mode(&self) -> Option<ToolExecutionMode> {
		Some(ToolExecutionMode::Sequential)
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
				"content": { "type": "string", "description": "Content to write to the file" }
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
			Ok(()) => AgentToolResult::text(format!("Successfully wrote to {path}")),
			Err(e) => AgentToolResult::error(format!("cannot write {path}: {e}")),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::{Path, PathBuf};

	fn tmp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-write-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	async fn run(cwd: &Path, params: Value) -> AgentToolResult {
		WriteTool::new(cwd.to_path_buf())
			.execute("test", params, CancellationToken::new(), None)
			.await
	}

	fn text(result: &AgentToolResult) -> String {
		result
			.content
			.iter()
			.filter_map(|c| c.text_content())
			.collect::<Vec<_>>()
			.join("\n")
	}

	#[tokio::test]
	async fn writes_new_file_and_creates_parents() {
		let dir = tmp_dir("new");
		let result = run(
			&dir,
			json!({"path": "a/b/c.txt", "content": "hello\nworld\n"}),
		)
		.await;
		assert!(!result.is_error, "{}", text(&result));
		assert_eq!(text(&result), "Successfully wrote to a/b/c.txt");
		assert_eq!(
			std::fs::read_to_string(dir.join("a/b/c.txt")).unwrap(),
			"hello\nworld\n"
		);
	}

	#[tokio::test]
	async fn overwrites_existing_file() {
		let dir = tmp_dir("overwrite");
		std::fs::write(dir.join("f.txt"), "old").unwrap();
		let result = run(&dir, json!({"path": "f.txt", "content": "new"})).await;
		assert!(!result.is_error, "{}", text(&result));
		assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "new");
	}

	#[tokio::test]
	async fn missing_params_error() {
		let dir = tmp_dir("params");
		let result = run(&dir, json!({"path": "f.txt"})).await;
		assert!(result.is_error);
		assert_eq!(text(&result), "missing required string parameter: content");
	}

	#[test]
	fn write_forces_sequential_tool_batch() {
		assert_eq!(
			WriteTool::new(PathBuf::new()).execution_mode(),
			Some(ToolExecutionMode::Sequential)
		);
	}
}
