//! `bash` tool — port of `pi/packages/coding-agent/src/core/tools/bash.ts`.
//! Uses pi-env's `LocalEnv` so the same tool runs over SSH later. The
//! hook machinery from the TS original lands with the shell-integration
//! phase; output truncation keeps the pi-rs 60k byte cap (upstream truncates
//! by lines + bytes via `OutputAccumulator`) and spills the full stream to a
//! temp file like upstream's `full_output_path`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdate};
use pi_env::{EnvConnection, ExecOptions, OutputChunkCallback};

use super::{param_str, param_u64_or};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT_CHARS: usize = 60_000;
// Leading-edge throttle for partial updates — `BASH_UPDATE_THROTTLE_MS` in
// `pi/packages/coding-agent/src/core/tools/renderers/bash.ts`: the first
// chunk emits immediately, later chunks coalesce into at most one update per
// window, and the newest text is always flushed at the end.
const UPDATE_THROTTLE: std::time::Duration = std::time::Duration::from_millis(500);

struct UpdateTask(tokio::task::JoinHandle<()>);

impl Drop for UpdateTask {
	fn drop(&mut self) {
		self.0.abort();
	}
}

/// Session metadata exposed to commands as `PI_RS_*` environment variables —
/// port of the `exposeSessionEnvironment` block in `core/tools/bash.ts`
/// (upstream uses the `PI_` prefix). All fields optional; a missing value is
/// simply not injected.
#[derive(Debug, Default, Clone)]
pub struct SessionToolContext {
	pub session_id: Option<String>,
	pub session_file: Option<String>,
	pub provider: Option<String>,
	pub model: Option<String>,
	pub reasoning_level: Option<String>,
}

impl SessionToolContext {
	fn env_vars(&self) -> Vec<(String, String)> {
		let mut vars = Vec::new();
		for (key, value) in [
			("PI_RS_SESSION_ID", &self.session_id),
			("PI_RS_SESSION_FILE", &self.session_file),
			("PI_RS_PROVIDER", &self.provider),
			("PI_RS_MODEL", &self.model),
			("PI_RS_REASONING_LEVEL", &self.reasoning_level),
		] {
			if let Some(value) = value {
				vars.push((key.to_string(), value.clone()));
			}
		}
		vars
	}
}

pub struct BashTool {
	cwd: PathBuf,
	session_context: SessionToolContext,
}

impl BashTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self {
			cwd,
			session_context: SessionToolContext::default(),
		}
	}

	/// Builder: inject session metadata as `PI_RS_*` env vars on every exec.
	pub fn with_session_context(mut self, ctx: SessionToolContext) -> Self {
		self.session_context = ctx;
		self
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
		// Wakes the throttled updater; each stdout chunk pings it so the
		// leading edge of the throttle is immediate.
		let chunk_wake = Arc::new(tokio::sync::Notify::new());
		let mut update_task = None;
		let captured = Arc::new(std::sync::Mutex::new((Vec::<u8>::new(), false)));
		if let Some(update) = on_update {
			let snapshot = captured.clone();
			let stop = stop_updates.clone();
			let wake = chunk_wake.clone();
			update_task = Some(UpdateTask(tokio::spawn(async move {
				let mut last_emit: Option<std::time::Instant> = None;
				loop {
					let finished = tokio::select! {
						_ = stop.cancelled() => true,
						_ = wake.notified() => false,
					};
					let due = last_emit
						.map(|since| since.elapsed() >= UPDATE_THROTTLE)
						.unwrap_or(true);
					let text = {
						let mut state = snapshot.lock().unwrap_or_else(|e| e.into_inner());
						if state.1 && (due || finished) {
							state.1 = false;
							Some(String::from_utf8_lossy(&state.0).into_owned())
						} else {
							None
						}
					};
					if let Some(text) = text {
						update(AgentToolResult::text(text));
						last_emit = Some(std::time::Instant::now());
					}
					if finished {
						break;
					}
				}
			})));
		}
		let output_buffer = captured.clone();
		let on_output = Some(Arc::new(move |chunk: &[u8]| {
			let mut state = output_buffer.lock().unwrap_or_else(|e| e.into_inner());
			let bytes = &mut state.0;
			bytes.extend_from_slice(chunk);
			let excess = bytes.len().saturating_sub(MAX_OUTPUT_CHARS);
			if excess > 0 {
				bytes.drain(..excess);
			}
			state.1 = true;
		}) as pi_env::connection::OutputCallback);
		let chunk_callback: OutputChunkCallback = match update_task.is_some() {
			true => {
				let wake = chunk_wake.clone();
				Arc::new(move |_chunk: &str| {
					// The captured text already carries the chunk; this ping
					// only drives the leading edge of the update throttle.
					wake.notify_one();
				})
			}
			// No update stream to feed — keep the no-op cheap.
			false => Arc::new(|_| {}),
		};
		let options = ExecOptions {
			cwd: Some(self.cwd.to_string_lossy().into_owned()),
			timeout_ms: Some(timeout_ms),
			env: self.session_context.env_vars(),
			on_output,
		};

		// Race the exec against the agent abort signal.
		let exec = env.exec_streaming(&command, options, chunk_callback);
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
					// Truncation spilled the complete stream to a temp file
					// (upstream's `full_output_path`).
					for path in [&output.full_stdout_path, &output.full_stderr_path]
						.into_iter()
						.flatten()
					{
						text.push('\n');
						text.push_str(&format!("(full output: {path})"));
					}
					// A signal-killed shell has no exit code; use the standard
					// 128+n shell convention so the failure is not mistaken for
					// a successful command (upstream maps signalCode the same way).
					if output.exit_code.is_none()
						&& let Some(sig) = output.signal
					{
						text.push('\n');
						text.push_str(&format!("terminated by signal {sig}"));
					}
					let failed = output.exit_code.map(|c| c != 0).unwrap_or(false)
						|| output.signal.is_some()
						|| output.timed_out;
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

	fn result_text(result: &AgentToolResult) -> String {
		result
			.content
			.iter()
			.filter_map(|c| c.text_content())
			.collect::<Vec<_>>()
			.join("\n")
	}

	#[tokio::test]
	async fn session_context_is_injected_as_pi_rs_env_vars() {
		let ctx = SessionToolContext {
			session_id: Some("sess-1".into()),
			session_file: Some("/tmp/session.jsonl".into()),
			provider: Some("mock".into()),
			model: Some("mock-echo".into()),
			reasoning_level: Some("high".into()),
		};
		let tool = BashTool::new(std::env::current_dir().unwrap()).with_session_context(ctx);
		let result = tool
			.execute(
				"test",
				json!({"command":"echo \"$PI_RS_SESSION_ID|$PI_RS_SESSION_FILE|$PI_RS_PROVIDER|$PI_RS_MODEL|$PI_RS_REASONING_LEVEL\""}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		assert_eq!(
			result_text(&result),
			"sess-1|/tmp/session.jsonl|mock|mock-echo|high"
		);
	}

	#[tokio::test]
	async fn missing_session_context_fields_are_not_injected() {
		let ctx = SessionToolContext {
			session_id: Some("sess-2".into()),
			..SessionToolContext::default()
		};
		let tool = BashTool::new(std::env::current_dir().unwrap()).with_session_context(ctx);
		let result = tool
			.execute(
				"test",
				json!({"command":"echo \"$PI_RS_SESSION_ID\"; [ -z \"$PI_RS_MODEL\" ] && [ -z \"$PI_RS_PROVIDER\" ] && [ -z \"$PI_RS_SESSION_FILE\" ] && [ -z \"$PI_RS_REASONING_LEVEL\" ] && echo unset"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error, "text: {}", result_text(&result));
		assert_eq!(result_text(&result), "sess-2\nunset");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn signal_termination_fails_with_128_plus_signal() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"printf partial; kill -TERM $$"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(result.is_error);
		let text = result_text(&result);
		assert!(text.contains("partial"));
		assert!(text.contains("terminated by signal 15"), "text: {text}");
	}

	#[tokio::test]
	async fn truncated_output_is_spilled_to_a_temp_file() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"yes x | head -c 80000"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		let text = result_text(&result);
		let path = text
			.lines()
			.find_map(|line| line.strip_prefix("(full output: "))
			.and_then(|rest| rest.strip_suffix(')'))
			.expect("result notes the full output path");
		let full = std::fs::read_to_string(path).unwrap();
		assert_eq!(full.len(), 80_000, "spill holds the complete stream");
		assert!(full.ends_with("x\n"));
		std::fs::remove_file(path).unwrap();
	}

	#[tokio::test]
	async fn fitting_output_is_not_spilled() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"echo small"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result_text(&result).contains("(full output:"));
	}
}
