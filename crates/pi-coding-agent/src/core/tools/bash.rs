//! `bash` tool — port of `pi/packages/coding-agent/src/core/tools/bash.ts`.
//! Uses pi-env's `LocalEnv` so the same tool runs over SSH later. The
//! hook/timeout-broadcast machinery from the TS original lands with the
//! shell-integration phase.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdate};
use pi_env::{EnvConnection, ExecOptions};

use super::{param_str, param_u64_or};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT_CHARS: usize = 60_000;
const UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

struct UpdateTask(tokio::task::JoinHandle<()>);

impl Drop for UpdateTask {
	fn drop(&mut self) {
		self.0.abort();
	}
}

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
		let mut end = MAX_OUTPUT_CHARS;
		while !s.is_char_boundary(end) {
			end -= 1;
		}
		format!(
			"{}\n… (output truncated at {} bytes)",
			&s[..end],
			MAX_OUTPUT_CHARS
		)
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
		on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let command = match param_str(&params, "command") {
			Ok(c) => c,
			Err(e) => return *e,
		};
		let timeout_ms = param_u64_or(&params, "timeout_ms", DEFAULT_TIMEOUT_MS);

		let env = pi_env::LocalEnv::with_cwd(self.cwd.to_string_lossy().into_owned());
		let stop_updates = CancellationToken::new();
		let mut update_task = None;
		let captured = std::sync::Arc::new(std::sync::Mutex::new((Vec::<u8>::new(), false)));
		if let Some(update) = on_update {
			let snapshot = captured.clone();
			let stop = stop_updates.clone();
			update_task = Some(UpdateTask(tokio::spawn(async move {
				let mut interval = tokio::time::interval(UPDATE_INTERVAL);
				interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
				loop {
					let finished = tokio::select! {
						_ = stop.cancelled() => true,
						_ = interval.tick() => false,
					};
					let text = {
						let mut state = snapshot.lock().unwrap_or_else(|e| e.into_inner());
						if state.1 {
							state.1 = false;
							Some(String::from_utf8_lossy(&state.0).into_owned())
						} else {
							None
						}
					};
					if let Some(text) = text {
						update(AgentToolResult::text(text));
					}
					if finished {
						break;
					}
				}
			})));
		}
		let output_buffer = captured.clone();
		let on_output = Some(std::sync::Arc::new(move |chunk: &[u8]| {
			let mut state = output_buffer.lock().unwrap_or_else(|e| e.into_inner());
			let bytes = &mut state.0;
			bytes.extend_from_slice(chunk);
			let excess = bytes.len().saturating_sub(MAX_OUTPUT_CHARS);
			if excess > 0 {
				bytes.drain(..excess);
			}
			state.1 = true;
		}) as pi_env::connection::OutputCallback);
		let options = ExecOptions {
			cwd: Some(self.cwd.to_string_lossy().into_owned()),
			timeout_ms: Some(timeout_ms),
			env: Vec::new(),
			on_output,
		};

		// Race the exec against the agent abort signal.
		let exec = env.exec(&command, options);
		let result = tokio::select! {
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
						AgentToolResult::error(format!("command timed out after {timeout_ms}ms\n{text}"))
					} else {
						let mut result = AgentToolResult::text(text);
						result.is_error = failed;
						result
					}
				}
				Err(e) => AgentToolResult::error(format!("exec failed: {e}")),
			},
			_ = signal.cancelled() => AgentToolResult::error("command aborted"),
		};
		stop_updates.cancel();
		if let Some(mut task) = update_task {
			let _ = (&mut task.0).await;
		}
		if result.is_error {
			let state = captured.lock().unwrap_or_else(|e| e.into_inner());
			let text = String::from_utf8_lossy(&state.0);
			if !text.trim().is_empty() {
				let message = result
					.content
					.iter()
					.filter_map(|c| c.text_content())
					.collect::<Vec<_>>()
					.join("\n");
				if message.starts_with("exec failed:") || message == "command aborted" {
					return AgentToolResult::error(format!("{message}\n{text}"));
				}
			}
		}
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn truncates_on_utf8_boundary() {
		let text = format!("{}中文", "a".repeat(MAX_OUTPUT_CHARS - 1));
		let truncated = truncate_output(&text);
		assert!(truncated.starts_with(&"a".repeat(MAX_OUTPUT_CHARS - 1)));
		assert!(truncated.contains("output truncated"));
	}

	#[tokio::test]
	async fn emits_output_before_command_finishes() {
		let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
		let callback: ToolUpdate = std::sync::Arc::new(move |result| {
			let text = result
				.content
				.iter()
				.filter_map(|c| c.text_content())
				.collect::<Vec<_>>()
				.join("\n");
			let _ = tx.send(text);
		});
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let run = tool.execute(
			"test",
			json!({"command":"printf first; sleep 1; printf second"}),
			CancellationToken::new(),
			Some(callback),
		);
		tokio::pin!(run);
		let first = tokio::select! {
			result = &mut run => panic!("command completed before update: {result:?}"),
			update = tokio::time::timeout(std::time::Duration::from_millis(800), rx.recv()) => update.unwrap().unwrap(),
		};
		assert!(first.contains("first"));
		let result = run.await;
		assert!(result.content[0].text_content().unwrap().contains("second"));
	}

	#[tokio::test]
	async fn frequent_output_is_coalesced_and_final_update_is_flushed() {
		let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
		let recorded = updates.clone();
		let callback: ToolUpdate = std::sync::Arc::new(move |result| {
			recorded
				.lock()
				.unwrap()
				.push(result.content[0].text_content().unwrap().to_owned());
		});
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"i=0; while [ $i -lt 1000 ]; do printf x; i=$((i+1)); done; printf END"}),
				CancellationToken::new(),
				Some(callback),
			)
			.await;
		assert!(!result.is_error);
		let updates = updates.lock().unwrap();
		assert!(!updates.is_empty());
		assert!(
			updates.len() <= 3,
			"updates were not coalesced: {}",
			updates.len()
		);
		assert!(updates.last().unwrap().ends_with("END"));
	}

	#[tokio::test]
	async fn timeout_and_abort_include_partial_output_without_update_callback() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"printf before; sleep 5", "timeout_ms":200}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(result.is_error);
		assert!(result.content[0].text_content().unwrap().contains("before"));
		let signal = CancellationToken::new();
		let trigger = signal.clone();
		tokio::spawn(async move {
			tokio::time::sleep(std::time::Duration::from_millis(200)).await;
			trigger.cancel();
		});
		let result = tool
			.execute(
				"test",
				json!({"command":"printf before; sleep 5"}),
				signal,
				None,
			)
			.await;
		assert!(result.is_error);
		assert!(result.content[0].text_content().unwrap().contains("before"));
	}

	#[tokio::test]
	async fn dropping_execution_releases_update_task() {
		let marker = std::sync::Arc::new(());
		let weak = std::sync::Arc::downgrade(&marker);
		let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
		let callback: ToolUpdate = std::sync::Arc::new(move |_| {
			let _ = &marker;
			let _ = tx.send(());
		});
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let mut run = Box::pin(tool.execute(
			"test",
			json!({"command":"printf ready; sleep 5"}),
			CancellationToken::new(),
			Some(callback),
		));
		tokio::select! {
			_ = &mut run => panic!("command exited early"),
			_ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()) => {}
		}
		drop(run);
		tokio::time::timeout(std::time::Duration::from_secs(1), async {
			while weak.upgrade().is_some() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("update task retained callback after execute dropped");
	}
}
