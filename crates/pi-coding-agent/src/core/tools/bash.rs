//! `bash` tool — port of `pi/packages/coding-agent/src/core/tools/bash.ts`.
//!
//! Uses pi-env's `LocalEnv` so the same tool runs over SSH later. Wire
//! schema matches upstream: `command` (required) plus an optional `timeout`
//! in **seconds** — no default timeout (omitting it runs unbounded), capped
//! at `2^31-1` ms. Output truncation follows upstream `truncate.ts`: the
//! **tail** (newest content) is kept, 2000 lines or 50KB whichever is hit
//! first, and the full stream is available in a temp file referenced by the
//! truncation notice. `structured_content` mirrors upstream's
//! `BashToolOutput` (`output` capped at 1MB with a middle-omission marker,
//! `exit_code`, `wall_time_seconds`).
//!
//! Accepted differences vs upstream (recorded, not blocking):
//! - pi-env captures stdout/stderr separately (upstream merges both streams
//!   in arrival order). pi-rs shows stdout first, then a `stderr:` section,
//!   and the "Full output" path may name two spill files (stdout, stderr).
//! - No `commandPrefix`/`spawnHook`/stdin-transport machinery (WSL legacy
//!   transport); no shell spawn hooks (extension system, P2).
//! - `PI_RS_*` env prefix instead of `PI_*` (accepted difference, declared
//!   in the P0 audit).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdate};
use pi_env::{EnvConnection, ExecOptions, OutputChunkCallback};

use super::param_str;
use super::truncate::{
	self, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationResult, format_size,
};

/// `MAX_TIMEOUT_MS` upstream: `timeout` is capped at 2^31-1 ms.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// `STRUCTURED_OUTPUT_MAX_BYTES` upstream: limit of `structuredContent.output`.
const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// Rolling cap for the live-update/abort snapshot of the interleaved
/// stdout+stderr stream (pi-env's own capture cap is 60KB per stream).
const CAPTURED_ROLLING_BYTES: usize = 60_000;
/// How much of a spilled stream is kept in memory for the tail view. The
/// display window needs at most 50KB + one partial line; 256KB leaves margin.
const SPILL_TAIL_SCAN_BYTES: usize = 256 * 1024;
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

/// `resolveTimeoutMs` upstream: `timeout` arrives in seconds; `None` means
/// no timeout at all (upstream has no default), and the cap is `2^31-1` ms.
fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, String> {
	let Some(timeout) = timeout else {
		return Ok(None);
	};
	if !timeout.is_finite() || timeout <= 0.0 {
		return Err("Invalid timeout: must be a finite number of seconds".into());
	}
	let timeout_ms = timeout * 1000.0;
	if timeout_ms > MAX_TIMEOUT_MS as f64 {
		return Err(format!(
			"Invalid timeout: maximum is {} seconds",
			MAX_TIMEOUT_MS / 1000
		));
	}
	Ok(Some(timeout_ms as u64))
}

/// JS-style `String(timeout)`: integers render without a decimal point.
fn format_seconds(timeout: f64) -> String {
	if timeout.fract() == 0.0 {
		format!("{}", timeout as i64)
	} else {
		format!("{timeout}")
	}
}

/// `appendStatus` upstream: blank text collapses the separator.
fn append_status(text: &str, status: &str) -> String {
	if text.is_empty() {
		status.to_string()
	} else {
		format!("{text}\n\n{status}")
	}
}

/// One output stream's view for display: the full text when the stream fit
/// in memory, otherwise the tail of its spill file plus the true totals
/// (pi-env keeps only the first 60KB in memory and spills the complete
/// stream to a temp file once exceeded).
struct StreamView {
	text: String,
	total_lines: usize,
	total_bytes: usize,
	/// Bytes in the stream's final (possibly open) line.
	last_line_bytes: usize,
	/// False when `text` lost its front (spill tail scan trimmed it).
	complete: bool,
	spill_path: Option<String>,
}

impl StreamView {
	fn in_memory(text: String, spill_path: Option<String>) -> Self {
		let result = truncate::truncate_head(&text, usize::MAX, usize::MAX);
		Self {
			total_bytes: text.len(),
			last_line_bytes: text.rsplit('\n').next().unwrap_or("").len(),
			text,
			total_lines: result.total_lines,
			complete: true,
			spill_path,
		}
	}

	/// Scan a spill file for the tail window while counting the true totals.
	fn from_spill(path: &str) -> Option<Self> {
		use std::io::Read;
		let mut file = std::fs::File::open(path).ok()?;
		let mut total_bytes = 0usize;
		let mut newlines = 0usize;
		let mut last_line_bytes = 0usize;
		let mut tail: Vec<u8> = Vec::with_capacity(SPILL_TAIL_SCAN_BYTES * 2);
		let mut trimmed = false;
		let mut chunk = [0u8; 64 * 1024];
		loop {
			let n = file.read(&mut chunk).ok()?;
			if n == 0 {
				break;
			}
			total_bytes += n;
			for byte in chunk[..n].iter() {
				if *byte == b'\n' {
					newlines += 1;
					last_line_bytes = 0;
				} else {
					last_line_bytes += 1;
				}
			}
			tail.extend_from_slice(&chunk[..n]);
			if tail.len() > SPILL_TAIL_SCAN_BYTES {
				let mut start = tail.len() - SPILL_TAIL_SCAN_BYTES;
				// Resume at a UTF-8 character start (continuation bytes are
				// 10xxxxxx), like the upstream decoders.
				while start < tail.len() && (tail[start] & 0xC0) == 0x80 {
					start += 1;
				}
				tail.drain(..start);
				trimmed = true;
			}
		}
		let mut text = String::from_utf8_lossy(&tail).into_owned();
		if trimmed {
			// Keep the tail starting at a line boundary (upstream
			// `getSnapshotText`), so line counts line up with the totals.
			if let Some(pos) = text.find('\n') {
				text.drain(..=pos);
			}
		}
		let total_lines = if total_bytes == 0 {
			0
		} else {
			newlines + usize::from(tail.last() != Some(&b'\n'))
		};
		Some(Self {
			text,
			total_lines,
			total_bytes,
			last_line_bytes,
			complete: !trimmed,
			spill_path: Some(path.to_string()),
		})
	}

	fn view(text: String, spill_path: Option<String>) -> Self {
		match spill_path.as_deref().and_then(Self::from_spill) {
			Some(view) => view,
			None => Self::in_memory(text, spill_path),
		}
	}
}

/// Assemble the stdout/stderr sections the way the previous pi-rs output
/// did: stdout first, then a labeled `stderr:` section.
fn assemble_output(
	stdout_shown: bool,
	stdout_text: &str,
	stderr_shown: bool,
	stderr_text: &str,
) -> String {
	let mut text = String::new();
	if stdout_shown {
		text.push_str(stdout_text);
	}
	if stderr_shown {
		if !text.is_empty() {
			text.push('\n');
		}
		text.push_str("stderr:\n");
		text.push_str(stderr_text);
	}
	text
}

/// The truncation notice appended to truncated bash output, using the
/// upstream `formatOutput` wording with true full-stream totals.
fn truncation_notice(
	truncation: &TruncationResult,
	total_lines: usize,
	last_line_bytes: usize,
	full_output_path: &str,
) -> String {
	let start_line = total_lines.saturating_sub(truncation.output_lines) + 1;
	let end_line = total_lines;
	if truncation.last_line_partial {
		format!(
			"[Showing last {} of line {} (line is {}). Full output: {full_output_path}]",
			format_size(truncation.output_bytes),
			end_line,
			format_size(last_line_bytes),
		)
	} else {
		match truncation.truncated_by {
			Some(TruncatedBy::Lines) => {
				format!(
					"[Showing lines {start_line}-{end_line} of {total_lines}. Full output: {full_output_path}]"
				)
			}
			_ => format!(
				"[Showing lines {start_line}-{end_line} of {total_lines} ({} limit). Full output: {full_output_path}]",
				format_size(DEFAULT_MAX_BYTES)
			),
		}
	}
}

/// Write the complete output to a temp file for the truncation notice when
/// pi-env did not spill either stream (output between the 50KB display cap
/// and pi-env's 60KB capture cap).
fn write_full_output_file(text: &str) -> Option<String> {
	let path = std::env::temp_dir().join(format!(
		"pi-rs-bash-{}-{}.log",
		std::process::id(),
		pi_ai::types::now_ms()
	));
	std::fs::write(&path, text).ok()?;
	Some(path.to_string_lossy().into_owned())
}

/// Read at most `max_bytes` of a file, keeping head and tail around a
/// middle-omission marker (upstream `OutputAccumulator.readFullOutput`).
fn read_bounded(path: &str, max_bytes: usize) -> (String, bool) {
	let Ok(meta) = std::fs::metadata(path) else {
		return (String::new(), false);
	};
	let size = meta.len() as usize;
	if size <= max_bytes {
		return match std::fs::read(path) {
			Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), false),
			Err(_) => (String::new(), false),
		};
	}
	let head_bytes = max_bytes / 2;
	let tail_bytes = max_bytes - head_bytes;
	let mut file = match std::fs::File::open(path) {
		Ok(file) => file,
		Err(_) => return (String::new(), false),
	};
	use std::io::{Read, Seek, SeekFrom};
	let mut head = vec![0u8; head_bytes];
	let mut tail = vec![0u8; tail_bytes];
	if file.read_exact(&mut head).is_err() {
		return (String::new(), false);
	}
	if file
		.seek(SeekFrom::Start((size - tail_bytes) as u64))
		.is_err()
		|| file.read_exact(&mut tail).is_err()
	{
		return (String::new(), false);
	}
	// Cut at UTF-8 boundaries like the upstream decoders: the head holds
	// back an incomplete trailing sequence, the tail skips leading
	// continuation bytes of the character straddling the cut.
	let head_end = utf8_prefix_len(&head);
	let mut tail_start = 0;
	while tail_start < tail.len() && (tail[tail_start] & 0xC0) == 0x80 {
		tail_start += 1;
	}
	let omitted = size - head_bytes - tail_bytes;
	(
		format!(
			"{}\n\n[... {omitted} bytes omitted ...]\n\n{}",
			String::from_utf8_lossy(&head[..head_end]),
			String::from_utf8_lossy(&tail[tail_start..]),
		),
		true,
	)
}

/// Length of the longest prefix of `bytes` that ends on a complete UTF-8
/// character (a trailing incomplete multi-byte sequence is held back, like
/// the upstream streaming decoder).
fn utf8_prefix_len(bytes: &[u8]) -> usize {
	let mut i = bytes.len();
	let mut back = 0;
	while i > 0 && back < 4 {
		let lead = bytes[i - 1];
		if lead & 0xC0 != 0x80 {
			let expected = if lead < 0x80 {
				1
			} else if lead & 0xE0 == 0xC0 {
				2
			} else if lead & 0xF0 == 0xE0 {
				3
			} else {
				4
			};
			if bytes.len() - (i - 1) < expected {
				return i - 1;
			}
			return bytes.len();
		}
		i -= 1;
		back += 1;
	}
	bytes.len()
}

#[async_trait]
impl AgentTool for BashTool {
	fn name(&self) -> &str {
		"bash"
	}

	fn description(&self) -> &str {
		"Execute a bash command in the current working directory. Returns stdout \
and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit \
first). If truncated, full output is saved to a temp file. Optionally provide \
a timeout in seconds."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"command": { "type": "string", "description": "Shell command to execute" },
				"timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
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
		let timeout_seconds = params.get("timeout").and_then(Value::as_f64);
		let timeout_ms = match resolve_timeout_ms(timeout_seconds) {
			Ok(timeout_ms) => timeout_ms,
			Err(message) => return AgentToolResult::error(message),
		};

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
			let excess = bytes.len().saturating_sub(CAPTURED_ROLLING_BYTES);
			if excess > 0 {
				// Rolling tail: live updates and abort reports show the
				// newest output, like the upstream OutputAccumulator.
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
			// None = no timeout, exactly when the caller omitted `timeout`.
			timeout_ms,
			env: self.session_context.env_vars(),
			on_output,
		};

		let started_at = std::time::Instant::now();
		// Race the exec against the agent abort signal.
		let exec = env.exec_streaming(&command, options, chunk_callback);
		let result = tokio::select! {
			result = exec => match result {
				Ok(output) => {
					let stdout_view = StreamView::view(output.stdout.clone(), output.full_stdout_path.clone());
					let stderr_view = StreamView::view(output.stderr.clone(), output.full_stderr_path.clone());
					let stdout_shown = !stdout_view.text.is_empty();
					let stderr_shown = !stderr_view.text.is_empty();
					let full_text = assemble_output(stdout_shown, &stdout_view.text, stderr_shown, &stderr_view.text);

					// True totals over both full streams plus the label lines.
					let mut total_lines = stdout_view.total_lines + stderr_view.total_lines;
					let mut total_bytes = stdout_view.total_bytes + stderr_view.total_bytes;
					if stderr_shown {
						total_lines += 1; // "stderr:" label
						total_bytes += "stderr:\n".len();
						if stdout_shown {
							total_lines += 1; // separator newline
							total_bytes += 1;
						}
					}

					let needs_truncation = total_lines > DEFAULT_MAX_LINES
						|| total_bytes > DEFAULT_MAX_BYTES
						|| !stdout_view.complete
						|| !stderr_view.complete;
					let (display_text, details, _full_output_path) = if !needs_truncation {
						(full_text.clone(), None, None)
					} else {
						let tail = super::truncate::truncate_tail(&full_text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
						// Full output location: pi-env's spill files when the
						// stream exceeded its capture cap, otherwise a file
						// written here (the 50KB display cap sits below the
						// 60KB capture cap). `full_text` is complete in the
						// latter case because nothing was spilled.
						let spill_paths: Vec<String> = [stdout_view.spill_path.clone(), stderr_view.spill_path.clone()]
							.into_iter()
							.flatten()
							.collect();
						let path = match spill_paths.is_empty() {
							false => Some(spill_paths.join(", ")),
							true => write_full_output_file(&full_text),
						};
						let last_line_bytes = if stderr_shown { stderr_view.last_line_bytes } else { stdout_view.last_line_bytes };
						let notice = match &path {
							Some(path) => truncation_notice(&tail, total_lines, last_line_bytes, path),
							None => String::new(),
						};
						let details = path.as_ref().map(|_| json!({
							"truncation": {
								"truncated": true,
								"truncatedBy": match tail.truncated_by {
									Some(TruncatedBy::Lines) => "lines",
									Some(TruncatedBy::Bytes) | None => "bytes",
								},
								"totalLines": total_lines,
								"totalBytes": total_bytes,
								"outputLines": tail.output_lines,
								"outputBytes": tail.output_bytes,
								"lastLinePartial": tail.last_line_partial,
							},
							"fullOutputPath": path,
						}));
						(format!("{}\n\n{notice}", tail.content), details, path)
					};

					// A signal-killed shell has no exit code; use the standard
					// 128+n shell convention so the failure is not mistaken
					// for a successful command (upstream maps signalCode the
					// same way before reporting).
					let exit_code: i64 = output
						.exit_code
						.map(i64::from)
						.or_else(|| output.signal.map(|sig| 128 + i64::from(sig)))
						.unwrap_or(1);

					// structuredContent: the full output up to 1MB with a
					// middle-omission marker, for programmatic callers.
					let mut structured = String::new();
					let mut structured_truncated = false;
					if !output.stdout.is_empty() || output.full_stdout_path.is_some() {
						let (text, truncated) = match &output.full_stdout_path {
							Some(path) => read_bounded(path, STRUCTURED_OUTPUT_MAX_BYTES),
							None => (output.stdout.clone(), false),
						};
						structured.push_str(&text);
						structured_truncated |= truncated;
					}
					if !output.stderr.is_empty() || output.full_stderr_path.is_some() {
						let (text, truncated) = match &output.full_stderr_path {
							Some(path) => read_bounded(path, STRUCTURED_OUTPUT_MAX_BYTES),
							None => (output.stderr.clone(), false),
						};
						if !structured.is_empty() {
							structured.push_str("\nstderr:\n");
						}
						structured.push_str(&text);
						structured_truncated |= truncated;
					}
					if structured.len() > STRUCTURED_OUTPUT_MAX_BYTES {
						(structured, _) = read_bounded_from_string(&structured, STRUCTURED_OUTPUT_MAX_BYTES);
						structured_truncated = true;
					}
					let wall_time_seconds = (started_at.elapsed().as_millis() as f64 / 100.0).round() / 10.0;

					if output.timed_out {
						let seconds = timeout_seconds.map(format_seconds).unwrap_or_default();
						let message = append_status(&display_text, &format!("Command timed out after {seconds} seconds"));
						let mut error = AgentToolResult::error(message);
						error.structured_content = Some(json!({
							"output": structured,
							"truncated": structured_truncated,
							"exit_code": exit_code,
							"wall_time_seconds": wall_time_seconds,
						}));
						error
					} else if exit_code != 0 {
						let message =
							append_status(&display_text, &format!("Command exited with code {exit_code}"));
						let mut error = AgentToolResult::error(message);
						error.details = details;
						error.structured_content = Some(json!({
							"output": structured,
							"truncated": structured_truncated,
							"exit_code": exit_code,
							"wall_time_seconds": wall_time_seconds,
						}));
						error
					} else {
						let mut ok = AgentToolResult::text(display_text);
						ok.details = details;
						ok.structured_content = Some(json!({
							"output": structured,
							"truncated": structured_truncated,
							"exit_code": exit_code,
							"wall_time_seconds": wall_time_seconds,
						}));
						ok
					}
				}
				Err(e) => {
					// pi-env enforces the timeout itself and reports it as an
					// error: surface the partial output plus the upstream
					// timeout wording ("Command timed out after N seconds").
					// Difference vs upstream: the full partial stream is not
					// archived to a temp file on this path (only the rolling
					// capture is available).
					if let pi_env::Error::Timeout(ms) = &e {
						let partial = {
							let state = captured.lock().unwrap_or_else(|e| e.into_inner());
							String::from_utf8_lossy(&state.0).into_owned()
						};
						let tail = super::truncate::truncate_tail(&partial, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
						let seconds = format_seconds(*ms as f64 / 1000.0);
						AgentToolResult::error(append_status(
							tail.content.trim_end(),
							&format!("Command timed out after {seconds} seconds"),
						))
					} else {
						let state = captured.lock().unwrap_or_else(|e| e.into_inner());
						let text = String::from_utf8_lossy(&state.0);
						if !text.trim().is_empty() {
							AgentToolResult::error(format!("exec failed: {e}\n{text}"))
						} else {
							AgentToolResult::error(format!("exec failed: {e}"))
						}
					}
				}
			},
			_ = signal.cancelled() => {
				let state = captured.lock().unwrap_or_else(|e| e.into_inner());
				let text = String::from_utf8_lossy(&state.0).into_owned();
				AgentToolResult::error(append_status(&text, "Command aborted"))
			}
		};
		stop_updates.cancel();
		if let Some(mut task) = update_task {
			let _ = (&mut task.0).await;
		}
		result
	}
}

/// Middle-truncate an in-memory string to `max_bytes`, head half + tail half
/// around a `[... N bytes omitted ...]` marker.
fn read_bounded_from_string(text: &str, max_bytes: usize) -> (String, bool) {
	if text.len() <= max_bytes {
		return (text.to_string(), false);
	}
	let mut head_end = max_bytes / 2;
	while head_end > 0 && !text.is_char_boundary(head_end) {
		head_end -= 1;
	}
	let mut tail_start = text.len() - (max_bytes - head_end);
	while tail_start < text.len() && !text.is_char_boundary(tail_start) {
		tail_start += 1;
	}
	let omitted = tail_start - head_end;
	(
		format!(
			"{}\n\n[... {omitted} bytes omitted ...]\n\n{}",
			&text[..head_end],
			&text[tail_start..],
		),
		true,
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn timeout_resolution_matches_upstream() {
		assert_eq!(
			resolve_timeout_ms(None).unwrap(),
			None,
			"no timeout = unbounded"
		);
		assert_eq!(resolve_timeout_ms(Some(5.0)).unwrap(), Some(5_000));
		assert_eq!(resolve_timeout_ms(Some(0.2)).unwrap(), Some(200));
		assert_eq!(
			resolve_timeout_ms(Some(0.0)).unwrap_err(),
			"Invalid timeout: must be a finite number of seconds"
		);
		assert_eq!(
			resolve_timeout_ms(Some(-1.0)).unwrap_err(),
			"Invalid timeout: must be a finite number of seconds"
		);
		assert_eq!(
			resolve_timeout_ms(Some(f64::INFINITY)).unwrap_err(),
			"Invalid timeout: must be a finite number of seconds"
		);
		assert_eq!(
			resolve_timeout_ms(Some((MAX_TIMEOUT_MS as f64) / 1000.0)).unwrap(),
			Some(MAX_TIMEOUT_MS),
			"cap itself is allowed"
		);
		assert_eq!(
			resolve_timeout_ms(Some(2_147_484.0)).unwrap_err(),
			"Invalid timeout: maximum is 2147483 seconds"
		);
	}

	#[test]
	fn seconds_render_like_js_numbers() {
		assert_eq!(format_seconds(5.0), "5");
		assert_eq!(format_seconds(0.2), "0.2");
	}

	#[test]
	fn middle_truncation_bounded_read() {
		let (text, truncated) = read_bounded_from_string(&"a".repeat(2000), 1000);
		assert!(truncated);
		assert!(text.contains("[... 1000 bytes omitted ...]"));
		assert_eq!(
			text.len(),
			1000 + "\n\n[... 1000 bytes omitted ...]\n\n".len()
		);
		let (text, truncated) = read_bounded_from_string("small", 1000);
		assert!(!truncated);
		assert_eq!(text, "small");
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
	async fn timeout_and_abort_include_partial_output() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"printf before; sleep 5", "timeout":0.2}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(result.is_error);
		let text = result.content[0].text_content().unwrap();
		assert!(text.contains("before"));
		assert!(
			text.contains("Command timed out after 0.2 seconds"),
			"{text}"
		);

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
		let text = result.content[0].text_content().unwrap();
		assert!(text.contains("before"));
		assert!(text.contains("Command aborted"), "{text}");
	}

	#[tokio::test]
	async fn invalid_timeout_values_fail() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		for timeout in [0.0, -3.0] {
			let result = tool
				.execute(
					"test",
					json!({"command":"echo hi", "timeout": timeout}),
					CancellationToken::new(),
					None,
				)
				.await;
			assert!(result.is_error);
			assert_eq!(
				result.content[0].text_content().unwrap(),
				"Invalid timeout: must be a finite number of seconds"
			);
		}
		let result = tool
			.execute(
				"test",
				json!({"command":"echo hi", "timeout": 2_147_484.0}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert_eq!(
			result.content[0].text_content().unwrap(),
			"Invalid timeout: maximum is 2147483 seconds"
		);
	}

	#[tokio::test]
	async fn omitted_timeout_runs_unbounded() {
		// A >1s command succeeds with no timeout parameter (the old Rust
		// default of 120000ms is gone; upstream has no default at all).
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"sleep 1.2; echo done"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert!(result_text(&result).contains("done"));
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

	fn full_output_path(text: &str) -> String {
		text.lines()
			.find_map(|line| line.split("Full output: ").nth(1))
			.map(|rest| rest.trim_end_matches(']').to_string())
			.expect("result notes the full output path")
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
		// Upstream keeps the stream verbatim: `echo` output carries its
		// trailing newline.
		assert_eq!(
			result_text(&result),
			"sess-1|/tmp/session.jsonl|mock|mock-echo|high\n"
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
		assert_eq!(result_text(&result), "sess-2\nunset\n");
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
		assert!(
			text.contains("Command exited with code 143"),
			"text: {text}"
		);
		let structured = result
			.structured_content
			.expect("structured content present");
		assert_eq!(structured["exit_code"], 143);
	}

	#[tokio::test]
	async fn line_truncated_output_keeps_tail_with_notice() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"seq 1 3000"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		let text = result_text(&result);
		assert!(
			text.contains("[Showing lines 1001-3000 of 3000. Full output: "),
			"{text}"
		);
		// The tail keeps the newest lines.
		assert!(text.contains("\n3000"), "{text}");
		assert!(
			!text.starts_with("1\n"),
			"head lines must be dropped: {text}"
		);
		// Full output is archived (nothing spilled here; bash writes it).
		let path = full_output_path(&text);
		let full = std::fs::read_to_string(&path).unwrap();
		assert!(
			full.starts_with("1\n"),
			"archive holds the head too: {}",
			&full[..20]
		);
		std::fs::remove_file(path).unwrap();
	}

	#[tokio::test]
	async fn byte_truncated_output_names_the_byte_limit() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"i=0; while [ $i -lt 1000 ]; do printf '%0400d\\n' $i; i=$((i+1)); done"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		let text = result_text(&result);
		assert!(text.contains("(50.0KB limit). Full output: "), "{text}");
		assert!(text.contains(" of 1000 "), "{text}");
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
		let path = full_output_path(&text);
		let full = std::fs::read_to_string(&path).unwrap();
		assert_eq!(full.len(), 80_000, "spill holds the complete stream");
		assert!(full.ends_with("x\n"));
		std::fs::remove_file(path).unwrap();
	}

	#[tokio::test]
	async fn display_truncation_below_capture_cap_writes_its_own_archive() {
		// 55KB: above the 50KB display cap, below pi-env's 60KB spill cap.
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"yes x | head -c 55000"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		let text = result_text(&result);
		let path = full_output_path(&text);
		let full = std::fs::read_to_string(&path).unwrap();
		assert_eq!(full.len(), 55_000);
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
		assert!(!result_text(&result).contains("Full output:"));
	}

	#[tokio::test]
	async fn structured_content_carries_exit_code_and_full_output() {
		let tool = BashTool::new(std::env::current_dir().unwrap());
		let result = tool
			.execute(
				"test",
				json!({"command":"echo hello-structured; echo oops >&2; exit 3"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(result.is_error);
		let text = result_text(&result);
		assert!(text.contains("hello-structured"));
		assert!(text.contains("Command exited with code 3"), "{text}");
		let structured = result
			.structured_content
			.expect("structured content present");
		assert_eq!(structured["exit_code"], 3);
		assert_eq!(structured["truncated"], false);
		assert!(
			structured["output"]
				.as_str()
				.unwrap()
				.contains("hello-structured")
		);
		assert!(structured["output"].as_str().unwrap().contains("oops"));
		assert!(structured["wall_time_seconds"].as_f64().unwrap() >= 0.0);
	}
}
