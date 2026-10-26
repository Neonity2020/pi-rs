//! Built-in tool registry — port of `pi/packages/coding-agent/src/core/tools/`.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use pi_agent::{AgentTool, AgentToolResult};

pub const ALL_TOOL_NAMES: &[&str] =
	&["read", "write", "edit", "bash", "grep", "find", "ls"];

/// `createTools()` — build the active tool set for a working directory.
pub fn create_tools(cwd: impl Into<PathBuf>) -> Vec<Arc<dyn AgentTool>> {
	let cwd = cwd.into();
	vec![
		Arc::new(read::ReadTool::new(cwd.clone())),
		Arc::new(write::WriteTool::new(cwd.clone())),
		Arc::new(edit::EditTool::new(cwd.clone())),
		Arc::new(bash::BashTool::new(cwd.clone())),
		Arc::new(grep::GrepTool::new(cwd.clone())),
		Arc::new(find::FindTool::new(cwd.clone())),
		Arc::new(ls::LsTool::new(cwd)),
	]
}

/// Resolve a tool-provided path against the tool cwd, mirroring
/// `path-utils.ts` (absolute paths pass through, `~` expands).
pub fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
	let expanded = if let Some(rest) = path.strip_prefix("~/") {
		dirs::home_dir().unwrap_or_default().join(rest)
	} else {
		PathBuf::from(path)
	};
	if expanded.is_absolute() {
		expanded
	} else {
		cwd.join(expanded)
	}
}

/// Extract a required string parameter.
pub fn param_str(params: &Value, key: &str) -> Result<String, AgentToolResult> {
	params
		.get(key)
		.and_then(|v| v.as_str())
		.map(|s| s.to_string())
		.ok_or_else(|| AgentToolResult::error(format!("missing required string parameter: {key}")))
}

/// Extract an optional parameter of mixed type.
pub fn param_value(params: &Value, key: &str) -> Option<Value> {
	params.get(key).cloned()
}

pub fn param_str_or(params: &Value, key: &str, default: &str) -> String {
	params.get(key).and_then(|v| v.as_str()).unwrap_or(default).to_string()
}

pub fn param_u64_or(params: &Value, key: &str, default: u64) -> u64 {
	params.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}

pub fn param_bool_or(params: &Value, key: &str, default: bool) -> bool {
	params.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}
