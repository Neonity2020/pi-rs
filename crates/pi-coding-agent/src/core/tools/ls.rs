//! `ls` tool — port of `pi/packages/coding-agent/src/core/tools/ls.ts`.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};
use super::{param_str_or, param_u64_or, resolve_path};

/// `DEFAULT_LIMIT` upstream: default entry cap.
const DEFAULT_LIMIT: usize = 500;

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
		"List directory contents. Returns entries sorted alphabetically, with \
'/' suffix for directories. Includes dotfiles. Output is truncated to 500 \
entries or 50KB (whichever is hit first)."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string", "description": "Directory to list (default: current directory)" },
				"limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
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
		let effective_limit = param_u64_or(&params, "limit", DEFAULT_LIMIT as u64).max(1) as usize;
		let dir_path = resolve_path(&self.cwd, &path);

		if !dir_path.exists() {
			return AgentToolResult::error(format!("Path not found: {}", dir_path.display()));
		}
		if !dir_path.is_dir() {
			return AgentToolResult::error(format!("Not a directory: {}", dir_path.display()));
		}
		let entries = match tokio::fs::read_dir(&dir_path).await {
			Ok(entries) => entries,
			Err(e) => return AgentToolResult::error(format!("Cannot read directory: {e}")),
		};

		// Sort alphabetically, case-insensitive (upstream localeCompare;
		// pi-rs compares the lowercased names — a recorded nuance).
		let mut names: Vec<String> = Vec::new();
		let mut entries = entries;
		while let Ok(Some(entry)) = entries.next_entry().await {
			names.push(entry.file_name().to_string_lossy().into_owned());
		}
		names.sort_by_key(|name| name.to_lowercase());

		// Format entries with directory indicators; entries that cannot be
		// stat-ed are skipped (upstream `catch { continue }`).
		let mut results: Vec<String> = Vec::new();
		let mut entry_limit_reached = false;
		for name in names {
			if results.len() >= effective_limit {
				// Upstream flags the limit only when the loop actually broke
				// on more entries (unlike find/grep's `len >= limit` check).
				entry_limit_reached = true;
				break;
			}
			let full_path = dir_path.join(&name);
			match tokio::fs::metadata(&full_path).await {
				Ok(metadata) if metadata.is_dir() => results.push(format!("{name}/")),
				Ok(_) => results.push(name),
				Err(_) => continue,
			}
		}

		if results.is_empty() {
			return AgentToolResult::text("(empty directory)");
		}

		let raw_output = results.join("\n");
		// Byte truncation only — the entry count is already capped.
		let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
		let mut output = truncation.content;

		let mut notices: Vec<String> = Vec::new();
		if entry_limit_reached {
			notices.push(format!(
				"{effective_limit} entries limit reached. Use limit={} for more",
				effective_limit * 2
			));
		}
		if truncation.truncated {
			notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
		}
		if !notices.is_empty() {
			output.push_str(&format!("\n\n[{}]", notices.join(". ")));
		}

		let mut result = AgentToolResult::text(output);
		let mut details = json!({});
		if entry_limit_reached {
			details["entryLimitReached"] = json!(effective_limit);
		}
		if truncation.truncated {
			details["truncation"] = json!({
				"truncated": true,
				"truncatedBy": "bytes",
				"totalLines": truncation.total_lines,
				"totalBytes": truncation.total_bytes,
				"outputLines": truncation.output_lines,
				"outputBytes": truncation.output_bytes,
			});
		}
		if details.as_object().is_some_and(|o| !o.is_empty()) {
			result.details = Some(details);
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
			"pi-rs-ls-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	async fn run(cwd: &Path, params: Value) -> AgentToolResult {
		LsTool::new(cwd.to_path_buf())
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
	async fn lists_entries_with_dir_suffix_and_dotfiles() {
		let dir = tmp_dir("entries");
		std::fs::write(dir.join("file.txt"), "x").unwrap();
		std::fs::create_dir_all(dir.join("subdir")).unwrap();
		std::fs::write(dir.join(".dotfile"), "x").unwrap();

		let result = run(&dir, json!({})).await;
		let out = text(&result);
		assert_eq!(out, ".dotfile\nfile.txt\nsubdir/", "{out}");
	}

	#[tokio::test]
	async fn sorts_case_insensitively() {
		let dir = tmp_dir("sorting");
		std::fs::write(dir.join("Banana"), "x").unwrap();
		std::fs::write(dir.join("apple"), "x").unwrap();
		std::fs::write(dir.join("Cherry"), "x").unwrap();

		let result = run(&dir, json!({})).await;
		assert_eq!(text(&result), "apple\nBanana\nCherry");
	}

	#[tokio::test]
	async fn empty_directory_message() {
		let dir = tmp_dir("empty");
		let result = run(&dir, json!({})).await;
		assert_eq!(text(&result), "(empty directory)");
	}

	#[tokio::test]
	async fn limit_parameter_and_notice() {
		let dir = tmp_dir("limit");
		for i in 0..12 {
			std::fs::write(dir.join(format!("e{i:02}")), "x").unwrap();
		}
		let result = run(&dir, json!({"limit": 5})).await;
		let out = text(&result);
		let entries: Vec<&str> = out
			.lines()
			.filter(|l| !l.is_empty() && !l.starts_with('['))
			.collect();
		assert_eq!(entries.len(), 5, "{out}");
		assert!(
			out.contains("5 entries limit reached. Use limit=10 for more"),
			"{out}"
		);
		assert_eq!(entries, vec!["e00", "e01", "e02", "e03", "e04"]);
	}

	#[tokio::test]
	async fn byte_cap_truncates_with_notice() {
		let dir = tmp_dir("bytecap");
		// 250 entries x ~224-char names ≈ 56KB, over the 50KB cap but under
		// the 255-byte filename limit.
		for i in 0..250 {
			std::fs::write(dir.join(format!("{i:03}_{}", "y".repeat(220))), "x").unwrap();
		}
		let result = run(&dir, json!({})).await;
		let out = text(&result);
		assert!(out.contains("50.0KB limit reached"), "{out}");
	}

	#[tokio::test]
	async fn error_paths() {
		let dir = tmp_dir("errors");
		std::fs::write(dir.join("plain.txt"), "x").unwrap();
		let result = run(&dir, json!({"path": "no/such"})).await;
		assert!(result.is_error);
		assert!(text(&result).starts_with("Path not found:"));

		let result = run(&dir, json!({"path": "plain.txt"})).await;
		assert!(result.is_error);
		assert!(text(&result).starts_with("Not a directory:"));
	}
}
