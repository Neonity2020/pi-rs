//! `bash` tool — port of `pi/packages/coding-agent/src/core/tools/bash.ts`.
//! Uses pi-env's `LocalEnv` so the same tool runs over SSH later. The
//! hook/timeout-broadcast machinery from the TS original lands with the
//! shell-integration phase.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdate};
use pi_env::{EnvConnection, ExecOptions};

use super::{param_str, param_u64_or};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT_CHARS: usize = 60_000;

pub struct BashTool {
	cwd: PathBuf,
}

impl BashTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

fn truncate_output(s: &str) -> String {
	if s.len() <= MAX_OUTPUT_CHARS {
		s.to_string()
	} else {
		format!("{}\n… (output truncated at {} chars)", &s[..MAX_OUTPUT_CHARS], MAX_OUTPUT_CHARS)
	}
}

#[async_trait]
impl AgentTool for BashTool {
	fn name(&self) -> &str {
		"bash"
	}

	fn description(&self) -> &str {
		"Execute a bash command in the working directory and return stdout/stderr. \
Prefer the dedicated read/edit/write/grep/find tools for file operations; use \
bash for everything else (builds, tests, git, ...)."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"command": { "type": "string" },
				"timeout_ms": { "type": "number", "description": "Kill the command after this many ms (default 120000)" }
			},
			"required": ["command"]
		})
	}

	fn execution_mode(&self) -> Option<ToolExecutionMode> {
		// pi runs bash calls sequentially by default to keep side effects ordered.
		Some(ToolExecutionMode::Sequential)
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let command = match param_str(&params, "command") {
			Ok(c) => c,
			Err(e) => return e,
		};
		let timeout_ms = param_u64_or(&params, "timeout_ms", DEFAULT_TIMEOUT_MS);

		let env = pi_env::LocalEnv::with_cwd(self.cwd.to_string_lossy().into_owned());
		let options = ExecOptions {
			cwd: Some(self.cwd.to_string_lossy().into_owned()),
			timeout_ms: Some(timeout_ms),
			env: Vec::new(),
		};

		// Race the exec against the agent abort signal.
		let exec = env.exec(&command, options);
		tokio::select! {
			result = exec => match result {
				Ok(output) => {
					let mut text = String::new();
					if !output.stdout.trim().is_empty() {
						text.push_str(&truncate_output(output.stdout.trim()));
					}
					if !output.stderr.trim().is_empty() {
						if !text.is_empty() { text.push('\n'); }
						text.push_str("stderr:\n");
						text.push_str(&truncate_output(output.stderr.trim()));
					}
					if text.is_empty() {
						text = "(no output)".into();
					}
					let failed = output.exit_code.map(|c| c != 0).unwrap_or(false) || output.timed_out;
					if output.timed_out {
						return AgentToolResult::error(format!("command timed out after {timeout_ms}ms\n{text}"));
					}
					let mut result = AgentToolResult::text(text);
					result.is_error = failed;
					result
				}
				Err(e) => AgentToolResult::error(format!("exec failed: {e}")),
			},
			_ = signal.cancelled() => AgentToolResult::error("command aborted"),
		}
	}
}
