//! `read` tool — port of `pi/packages/coding-agent/src/core/tools/read.ts`.
//!
//! Truncation matches upstream `truncate.ts`: 2000 lines **or** 50KB,
//! whichever is hit first, with the upstream continuation notices
//! (`Use offset=N to continue`) and the bash fallback hint when the first
//! line alone exceeds the byte limit. Image support is P2 upstream parity.
//!
//! Accepted difference (declared in the P0 audit): pi-rs prefixes every
//! line with `cat -n`-style line numbers; upstream read output has no line
//! numbers. Line-number width uses the upstream 6-column gutter.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::truncate::{self, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size};
use super::{param_str, param_u64_or, resolve_path};

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
		"Read the contents of a file. For text files, output is truncated to \
2000 lines or 50KB (whichever is hit first). Use offset/limit for large \
files. When you need the full file, continue with offset until complete."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
				"offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
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
		let offset = param_u64_or(&params, "offset", 0);
		let limit = params.get("limit").and_then(Value::as_u64);
		let resolved = resolve_path(&self.cwd, &path);

		let content = match tokio::fs::read(&resolved).await {
			Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
			Err(e) => return AgentToolResult::error(format!("cannot read {path}: {e}")),
		};

		// JS `split("\n")` line accounting: a trailing newline does not open
		// a new line for slicing, but the total count includes the empty
		// final piece (upstream reports it in the offset error).
		let all_lines: Vec<&str> = content.split('\n').collect();
		let total_file_lines = all_lines.len();
		let start_line = usize::try_from(offset.saturating_sub(1)).unwrap_or(0);
		let start_line_display = start_line + 1;
		if start_line >= total_file_lines && offset > 0 {
			return AgentToolResult::error(format!(
				"Offset {offset} is beyond end of file ({total_file_lines} lines total)"
			));
		}

		// A user limit is honored first; otherwise truncateHead decides.
		let (selected, user_limited_lines) = match limit {
			Some(limit) => {
				let end_line = (start_line + limit as usize).min(total_file_lines);
				(
					all_lines[start_line..end_line].join("\n"),
					Some(end_line - start_line),
				)
			}
			None => (all_lines[start_line..].join("\n"), None),
		};

		let truncation = truncate::truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		let line_num_width = total_file_lines.to_string().len().max(6);
		let render_numbered = |lines: &[&str], first_line: usize| -> String {
			lines
				.iter()
				.enumerate()
				.map(|(i, line)| {
					format!("{:>width$}\t{line}", first_line + i, width = line_num_width)
				})
				.collect::<Vec<_>>()
				.join("\n")
		};

		let output_text: String;
		if truncation.first_line_exceeds_limit {
			// First line alone exceeds the byte limit: point the model at a
			// bash fallback (upstream wording).
			let first_line_size = all_lines[start_line].len();
			output_text = format!(
				"[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
				format_size(first_line_size),
				format_size(DEFAULT_MAX_BYTES),
			);
		} else if truncation.truncated {
			let end_line_display = start_line_display + truncation.output_lines - 1;
			let next_offset = end_line_display + 1;
			let numbered = render_numbered(
				&all_lines[start_line..start_line + truncation.output_lines],
				start_line_display,
			);
			output_text = match truncation.truncated_by {
				Some(TruncatedBy::Lines) => format!(
					"{numbered}\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
				),
				_ => format!(
					"{numbered}\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
					format_size(DEFAULT_MAX_BYTES)
				),
			};
		} else if let Some(user_limited_lines) = user_limited_lines
			&& start_line + user_limited_lines < total_file_lines
		{
			// User-specified limit stopped early, but the file has more.
			let remaining = total_file_lines - (start_line + user_limited_lines);
			let next_offset = start_line + user_limited_lines + 1;
			let numbered = render_numbered(
				&all_lines[start_line..start_line + user_limited_lines],
				start_line_display,
			);
			output_text = format!(
				"{numbered}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]"
			);
		} else {
			let shown = match user_limited_lines {
				Some(user_limited_lines) => user_limited_lines,
				None => truncation.output_lines,
			};
			output_text = render_numbered(
				&all_lines[start_line..start_line + shown],
				start_line_display,
			);
		}

		let mut result = AgentToolResult::text(output_text);
		if truncation.truncated {
			result.details = Some(json!({
				"truncation": {
					"truncated": true,
					"truncatedBy": match truncation.truncated_by {
						Some(TruncatedBy::Lines) => "lines",
						Some(TruncatedBy::Bytes) | None => "bytes",
					},
					"totalLines": truncation.total_lines,
					"totalBytes": truncation.total_bytes,
					"outputLines": truncation.output_lines,
					"outputBytes": truncation.output_bytes,
					"firstLineExceedsLimit": truncation.first_line_exceeds_limit,
				},
			}));
		}
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::{Path, PathBuf};

	fn tmp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-read-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	async fn run(cwd: &Path, params: Value) -> AgentToolResult {
		ReadTool::new(cwd.to_path_buf())
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
	async fn reads_with_line_number_prefix() {
		let dir = tmp_dir("numbered");
		std::fs::write(dir.join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
		let result = run(&dir, json!({"path": "f.txt"})).await;
		assert!(!result.is_error, "{}", text(&result));
		assert_eq!(text(&result), "     1\talpha\n     2\tbeta\n     3\tgamma");
	}

	#[tokio::test]
	async fn offset_and_limit_slice_the_file() {
		let dir = tmp_dir("slice");
		let content = (1..=30)
			.map(|i| format!("line{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		std::fs::write(dir.join("f.txt"), content).unwrap();

		let result = run(&dir, json!({"path": "f.txt", "offset": 10, "limit": 3})).await;
		let out = text(&result);
		assert!(out.contains("\tline10\n"), "{out}");
		assert!(out.contains("\tline12\n"), "{out}");
		assert!(
			out.contains("[18 more lines in file. Use offset=13 to continue.]"),
			"{out}"
		);
	}

	#[tokio::test]
	async fn line_cap_truncates_with_upstream_notice() {
		let dir = tmp_dir("linecap");
		let content = (1..=3000)
			.map(|i| format!("line{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		std::fs::write(dir.join("f.txt"), content).unwrap();

		let result = run(&dir, json!({"path": "f.txt"})).await;
		let out = text(&result);
		assert!(out.contains("\tline2000"), "{out}");
		assert!(!out.contains("\tline2001"), "{out}");
		assert!(
			out.contains("[Showing lines 1-2000 of 3000. Use offset=2001 to continue.]"),
			"{out}"
		);
	}

	#[tokio::test]
	async fn byte_cap_truncates_with_kb_notice() {
		let dir = tmp_dir("bytecap");
		let content = (1..=1000)
			.map(|i| format!("{i} {}", "x".repeat(200)))
			.collect::<Vec<_>>()
			.join("\n");
		std::fs::write(dir.join("f.txt"), content).unwrap();

		let result = run(&dir, json!({"path": "f.txt"})).await;
		let out = text(&result);
		assert!(out.contains("(50.0KB limit). Use offset="), "{out}");
		assert!(out.contains("of 1000 "), "{out}");
	}

	#[tokio::test]
	async fn oversized_first_line_gets_bash_fallback_hint() {
		let dir = tmp_dir("firstline");
		let content = format!("{}\nsecond line\n", "z".repeat(DEFAULT_MAX_BYTES + 2048));
		std::fs::write(dir.join("f.txt"), content).unwrap();

		let result = run(&dir, json!({"path": "f.txt"})).await;
		let out = text(&result);
		assert!(
			out.starts_with(
				"[Line 1 is 52.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' f.txt | head -c 51200]"
			),
			"{out}"
		);
	}

	#[tokio::test]
	async fn offset_beyond_end_errors() {
		let dir = tmp_dir("beyond");
		std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
		// JS split("\n") accounting: the trailing newline's empty piece
		// counts toward the total (upstream reports it the same way).
		let result = run(&dir, json!({"path": "f.txt", "offset": 10})).await;
		assert!(result.is_error);
		assert_eq!(
			text(&result),
			"Offset 10 is beyond end of file (3 lines total)"
		);
	}

	#[tokio::test]
	async fn empty_file_reads_as_empty_output() {
		let dir = tmp_dir("empty");
		std::fs::write(dir.join("f.txt"), "").unwrap();
		let result = run(&dir, json!({"path": "f.txt"})).await;
		assert!(!result.is_error);
		assert_eq!(text(&result), "");
	}

	#[tokio::test]
	async fn missing_file_errors() {
		let dir = tmp_dir("missing");
		let result = run(&dir, json!({"path": "nope.txt"})).await;
		assert!(result.is_error);
		assert!(text(&result).starts_with("cannot read nope.txt:"));
	}
}
