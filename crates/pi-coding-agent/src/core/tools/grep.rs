//! `grep` tool — port of `pi/packages/coding-agent/src/core/tools/grep.ts`.
//!
//! Upstream shells out to a managed ripgrep (`rg --no-config --json
//! --line-number --color=never --hidden [--ignore-case] [--fixed-strings]
//! [--glob GLOB] -- pattern path`); pi-rs walks with the `ignore` crate
//! (ripgrep's own walker: .gitignore/.ignore/global git excludes, git-repo
//! gating like rg's default) and matches with the `regex` crate in-process.
//! **Accepted implementation difference** (recorded in the P0 audit): rg
//! subprocess vs library walker — the observable parameters, defaults and
//! truncation limits below match the upstream tool contract.
//!
//! Glob filtering uses `globset` with rg's `--glob` semantics (`*` crosses
//! `/`, matching is against the path relative to the search root), which the
//! ignore crate's gitignore-style overrides deliberately do not provide.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::RegexBuilder;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::truncate::{
	DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, format_size, truncate_head, truncate_line,
};
use super::{param_bool_or, param_str_or, param_u64_or, resolve_path};

/// `DEFAULT_LIMIT` upstream: default match cap.
const DEFAULT_LIMIT: usize = 100;

pub struct GrepTool {
	cwd: PathBuf,
}

impl GrepTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

/// One recorded match before formatting.
struct Match {
	path: PathBuf,
	line_number: usize,
	/// Sanitized line text (display-ready), used when `context` is 0.
	line_text: String,
}

/// Split the `glob` parameter into include/exclude sets with rg `--glob`
/// semantics (a `!` prefix excludes; one or more includes means files must
/// match at least one).
struct GlobFilter {
	includes: Option<GlobSet>,
	excludes: GlobSet,
}

impl GlobFilter {
	fn from_globs(globs: &[String]) -> Result<Option<Self>, String> {
		if globs.is_empty() {
			return Ok(None);
		}
		let mut includes = GlobSetBuilder::new();
		let mut excludes = GlobSetBuilder::new();
		let mut has_include = false;
		for glob in globs {
			let (pattern, exclude) = match glob.strip_prefix('!') {
				Some(rest) => (rest, true),
				None => (glob.as_str(), false),
			};
			// rg matches `--glob` against the whole path with `*` crossing
			// separators (literal_separator(false)).
			let glob = GlobBuilder::new(pattern)
				.literal_separator(false)
				.build()
				.map_err(|e| format!("invalid glob `{glob}`: {e}"))?;
			if exclude {
				excludes.add(glob);
			} else {
				has_include = true;
				includes.add(glob);
			}
		}
		Ok(Some(Self {
			includes: has_include
				.then(|| includes.build().map_err(|e| e.to_string()))
				.transpose()?,
			excludes: excludes.build().map_err(|e| e.to_string())?,
		}))
	}

	fn keeps(&self, relative_path: &Path) -> bool {
		if let Some(includes) = &self.includes
			&& !includes.is_match(relative_path)
		{
			return false;
		}
		!self.excludes.is_match(relative_path)
	}
}

/// Build the gitignore-aware walker for a search root. `--hidden` upstream:
/// hidden entries are searched, but the `.git` directory never is.
fn build_walker(root: &Path) -> ignore::Walk {
	ignore::WalkBuilder::new(root)
		.hidden(false)
		.filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
		.build()
}

#[async_trait]
impl AgentTool for GrepTool {
	fn name(&self) -> &str {
		"grep"
	}

	fn description(&self) -> &str {
		"Search file contents for a pattern. Returns matching lines with file \
paths and line numbers. Respects .gitignore. Output is truncated to 100 \
matches or 50KB (whichever is hit first). Long lines are truncated to 500 \
chars."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
				"path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
				"glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
				"ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
				"literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
				"context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
				"limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
			},
			"required": ["pattern"]
		})
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let pattern = match params.get("pattern").and_then(Value::as_str) {
			Some(p) => p,
			None => return AgentToolResult::error("missing required string parameter: pattern"),
		};
		let path = param_str_or(&params, "path", ".");
		let glob_param = params.get("glob").and_then(Value::as_str);
		let ignore_case = param_bool_or(&params, "ignoreCase", false);
		let literal = param_bool_or(&params, "literal", false);
		let context = param_u64_or(&params, "context", 0) as usize;
		let effective_limit = param_u64_or(&params, "limit", DEFAULT_LIMIT as u64).max(1) as usize;

		let regex_source = if literal {
			regex::escape(pattern)
		} else {
			pattern.to_string()
		};
		let regex = match RegexBuilder::new(&regex_source)
			.case_insensitive(ignore_case)
			.build()
		{
			Ok(regex) => regex,
			Err(e) => return AgentToolResult::error(format!("invalid pattern: {e}")),
		};

		let search_path = resolve_path(&self.cwd, &path);
		if !search_path.exists() {
			return AgentToolResult::error(format!("Path not found: {}", search_path.display()));
		}
		let is_directory = search_path.is_dir();

		let glob_filter = match glob_param {
			Some(glob) => match GlobFilter::from_globs(&[glob.to_string()]) {
				Ok(filter) => filter,
				Err(message) => return AgentToolResult::error(message),
			},
			None => None,
		};

		// Collect matches (capped), keeping whole files in memory only when
		// context lines are requested (upstream's `formatBlock` re-reads
		// files through a cache for the same purpose).
		let mut matches: Vec<Match> = Vec::new();
		let mut file_lines: HashMap<PathBuf, Vec<String>> = HashMap::new();
		let mut match_limit_reached = false;

		{
			let record = |matches: &mut Vec<Match>,
			              path: PathBuf,
			              line_number: usize,
			              line_text: String,
			              limit: usize| {
				matches.push(Match {
					path,
					line_number,
					line_text,
				});
				matches.len() >= limit
			};

			if !is_directory {
				// Single-file search (upstream formats these with the basename).
				if scan_file(
					&search_path,
					&regex,
					&mut |line_number, line_text| {
						record(
							&mut matches,
							search_path.clone(),
							line_number,
							line_text,
							effective_limit,
						)
					},
					if context > 0 {
						Some(&mut file_lines)
					} else {
						None
					},
				) == Some(true)
				{
					match_limit_reached = true;
				}
			} else {
				for entry in build_walker(&search_path) {
					if signal.is_cancelled() {
						return AgentToolResult::error("Operation aborted");
					}
					let Ok(entry) = entry else { continue };
					if !entry.file_type().is_some_and(|t| t.is_file()) {
						continue;
					}
					if let Some(filter) = &glob_filter {
						let relative = entry
							.path()
							.strip_prefix(&search_path)
							.unwrap_or(entry.path());
						if !filter.keeps(relative) {
							continue;
						}
					}
					let path = entry.path().to_path_buf();
					if scan_file(
						&path,
						&regex,
						&mut |line_number, line_text| {
							record(
								&mut matches,
								path.clone(),
								line_number,
								line_text,
								effective_limit,
							)
						},
						if context > 0 {
							Some(&mut file_lines)
						} else {
							None
						},
					) == Some(true)
					{
						match_limit_reached = true;
						break;
					}
				}
			}
		}

		if matches.is_empty() {
			return AgentToolResult::text("No matches found");
		}

		// Format: with context, each match becomes a block over the file's
		// lines; without, one `path:line: text` row per match (upstream).
		let format_path = |path: &Path| -> String {
			if is_directory && let Ok(relative) = path.strip_prefix(&search_path) {
				return relative.to_string_lossy().replace('\\', "/");
			}
			path.file_name()
				.map(|name| name.to_string_lossy().into_owned())
				.unwrap_or_else(|| path.to_string_lossy().into_owned())
		};

		let mut output_rows: Vec<String> = Vec::new();
		let mut lines_truncated = false;
		for matched in &matches {
			let relative = format_path(&matched.path);
			if context == 0 {
				let (text, was_truncated) = truncate_line(&matched.line_text, GREP_MAX_LINE_LENGTH);
				lines_truncated |= was_truncated;
				output_rows.push(format!("{relative}:{}: {text}", matched.line_number));
			} else {
				let lines = match file_lines.get(&matched.path) {
					Some(lines) if !lines.is_empty() => lines,
					_ => {
						output_rows.push(format!(
							"{relative}:{}: (unable to read file)",
							matched.line_number
						));
						continue;
					}
				};
				let start = matched.line_number.saturating_sub(context).max(1);
				let end = (matched.line_number + context).min(lines.len());
				for current in start..=end {
					let line_text = lines[current - 1].clone();
					let (text, was_truncated) = truncate_line(&line_text, GREP_MAX_LINE_LENGTH);
					lines_truncated |= was_truncated;
					if current == matched.line_number {
						output_rows.push(format!("{relative}:{current}: {text}"));
					} else {
						output_rows.push(format!("{relative}-{current}- {text}"));
					}
				}
			}
		}

		let raw_output = output_rows.join("\n");
		// Byte truncation only — the match limit already capped the rows
		// (upstream truncates with `maxLines: Number.MAX_SAFE_INTEGER`).
		let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
		let mut output = truncation.content;

		let mut notices: Vec<String> = Vec::new();
		if match_limit_reached {
			notices.push(format!(
				"{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
				effective_limit * 2
			));
		}
		if truncation.truncated {
			notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
		}
		if lines_truncated {
			notices.push(format!(
				"Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
			));
		}
		if !notices.is_empty() {
			output.push_str(&format!("\n\n[{}]", notices.join(". ")));
		}

		let mut result = AgentToolResult::text(output);
		let mut details = json!({});
		if match_limit_reached {
			details["matchLimitReached"] = json!(effective_limit);
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
		if lines_truncated {
			details["linesTruncated"] = json!(true);
		}
		if details.as_object().is_some_and(|o| !o.is_empty()) {
			result.details = Some(details);
		}
		result
	}
}

/// Scan one file for regex matches. `record` returns whether the match
/// limit was hit. `store_lines` keeps the file's lines for context
/// formatting. Returns `Some(true)` when the limit was hit; `Some(false)`
/// for a binary file (scan stops early, like rg's binary detection); `None`
/// when the file could not be read.
fn scan_file(
	path: &Path,
	regex: &regex::Regex,
	record: &mut dyn FnMut(usize, String) -> bool,
	store_lines: Option<&mut HashMap<PathBuf, Vec<String>>>,
) -> Option<bool> {
	let file = std::fs::File::open(path).ok()?;
	let mut reader = BufReader::new(file);
	let mut raw: Vec<u8> = Vec::new();
	let mut lines: Vec<String> = Vec::new();
	let mut line_number = 0usize;
	let mut limit_hit = false;
	loop {
		raw.clear();
		if reader.read_until(b'\n', &mut raw).ok()? == 0 {
			break;
		}
		line_number += 1;
		// Binary files are skipped, like rg's default binary detection.
		if raw.contains(&0) {
			return Some(false);
		}
		// Match the raw line (trailing newline stripped, any \r kept, like
		// rg's default); the sanitized text is stored for display.
		let mut line = String::from_utf8_lossy(&raw).into_owned();
		while line.ends_with('\n') || line.ends_with('\r') {
			line.pop();
		}
		if store_lines.is_some() || regex.is_match(&line) {
			let sanitized = line.replace('\r', "");
			if regex.is_match(&line) && record(line_number, sanitized.clone()) {
				limit_hit = true;
				break;
			}
			if store_lines.is_some() {
				lines.push(sanitized);
			}
		}
	}
	if let Some(store) = store_lines {
		store.insert(path.to_path_buf(), lines);
	}
	Some(limit_hit)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	fn tmp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-grep-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	async fn run(cwd: &Path, params: Value) -> AgentToolResult {
		GrepTool::new(cwd.to_path_buf())
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
	async fn finds_matches_with_line_numbers_and_respects_gitignore() {
		let dir = tmp_dir("gitignore");
		// rg (and the ignore crate) only apply .gitignore inside a git
		// repository — a `.git` entry marks one.
		std::fs::create_dir_all(dir.join(".git")).unwrap();
		std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
		std::fs::write(dir.join("a.txt"), "needle here\nother\nneedle two\n").unwrap();
		std::fs::create_dir_all(dir.join("sub")).unwrap();
		std::fs::write(dir.join("sub/b.txt"), "no hit\n").unwrap();
		std::fs::write(dir.join("ignored.txt"), "needle in ignored\n").unwrap();

		let result = run(&dir, json!({"pattern": "needle"})).await;
		let out = text(&result);
		assert!(out.contains("a.txt:1: needle here"), "{out}");
		assert!(out.contains("a.txt:3: needle two"), "{out}");
		assert!(!out.contains("ignored"), "{out}");
		assert!(!result.is_error);
	}

	#[tokio::test]
	async fn no_matches_message() {
		let dir = tmp_dir("nomatch");
		std::fs::write(dir.join("a.txt"), "nothing here\n").unwrap();
		let result = run(&dir, json!({"pattern": "zzzznotfound"})).await;
		assert_eq!(text(&result), "No matches found");
	}

	#[tokio::test]
	async fn literal_ignore_case_and_glob_parameters() {
		let dir = tmp_dir("params");
		std::fs::write(dir.join("a.rs"), "findMe(1);\n").unwrap();
		std::fs::write(dir.join("b.txt"), "findMe(2);\n").unwrap();

		// glob filter: only .rs files.
		let result = run(&dir, json!({"pattern": "findMe", "glob": "*.rs"})).await;
		let out = text(&result);
		assert!(out.contains("a.rs:1:"), "{out}");
		assert!(!out.contains("b.txt"), "{out}");

		// glob with path separator matches nested files (rg semantics).
		std::fs::create_dir_all(dir.join("deep/nested")).unwrap();
		std::fs::write(dir.join("deep/nested/c.txt"), "findMe(3);\n").unwrap();
		let result = run(&dir, json!({"pattern": "findMe", "glob": "*.txt"})).await;
		let out = text(&result);
		assert!(out.contains("deep/nested/c.txt:1:"), "{out}");

		// literal: regex metacharacters lose their meaning.
		std::fs::write(dir.join("c.txt"), "a.c\nabc\n").unwrap();
		let result = run(&dir, json!({"pattern": "a.c", "literal": true})).await;
		let out = text(&result);
		assert!(out.contains("c.txt:1: a.c"), "{out}");
		assert!(!out.contains("abc"), "{out}");
		let result = run(&dir, json!({"pattern": "a.c"})).await;
		let out = text(&result);
		assert!(out.contains("abc"), "{out}");

		// ignoreCase.
		let result = run(&dir, json!({"pattern": "FINDME", "ignoreCase": true})).await;
		assert!(text(&result).contains("findMe"), "{}", text(&result));
	}

	#[tokio::test]
	async fn context_lines_around_matches() {
		let dir = tmp_dir("context");
		std::fs::write(dir.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
		let result = run(&dir, json!({"pattern": "three", "context": 1})).await;
		let out = text(&result);
		assert!(
			out.contains("f.txt-1- one") || out.contains("f.txt-2- two"),
			"{out}"
		);
		assert!(out.contains("f.txt:3: three"), "{out}");
		assert!(out.contains("f.txt-4- four"), "{out}");
	}

	#[tokio::test]
	async fn default_limit_is_100_and_notice_suggests_double() {
		let dir = tmp_dir("limit");
		let content = (0..250)
			.map(|i| format!("hit{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		std::fs::write(dir.join("f.txt"), content).unwrap();

		// Default cap of 100.
		let result = run(&dir, json!({"pattern": "hit"})).await;
		let out = text(&result);
		assert_eq!(
			out.lines().filter(|l| l.contains("f.txt:")).count(),
			100,
			"{out}"
		);
		assert!(
			out.contains("100 matches limit reached. Use limit=200 for more"),
			"{out}"
		);

		// Explicit limit raises the cap.
		let result = run(&dir, json!({"pattern": "hit", "limit": 150})).await;
		assert_eq!(
			text(&result)
				.lines()
				.filter(|l| l.contains("f.txt:"))
				.count(),
			150
		);
	}

	#[tokio::test]
	async fn long_lines_are_truncated() {
		let dir = tmp_dir("longlines");
		std::fs::write(dir.join("f.txt"), format!("{}NEEDLE\n", "x".repeat(600))).unwrap();
		let result = run(&dir, json!({"pattern": "NEEDLE"})).await;
		let out = text(&result);
		assert!(out.contains("... [truncated]"), "{out}");
		assert!(out.contains("Some lines truncated to 500 chars"), "{out}");
	}

	#[tokio::test]
	async fn binary_files_are_skipped() {
		let dir = tmp_dir("binary");
		std::fs::write(dir.join("bin.dat"), b"needle\x00needle".as_slice()).unwrap();
		let result = run(&dir, json!({"pattern": "needle"})).await;
		assert_eq!(text(&result), "No matches found");
	}

	#[tokio::test]
	async fn single_file_search_uses_basename() {
		let dir = tmp_dir("single");
		let file = dir.join("only.txt");
		std::fs::write(&file, "hit one\nmiss\nhit two\n").unwrap();
		let result = run(&dir, json!({"pattern": "hit", "path": "only.txt"})).await;
		let out = text(&result);
		assert!(out.contains("only.txt:1: hit one"), "{out}");
		assert!(out.contains("only.txt:3: hit two"), "{out}");
	}

	#[tokio::test]
	async fn byte_cap_truncates_with_notice() {
		let dir = tmp_dir("bytecap");
		// 200 rows of ~300 bytes = ~60KB > 50KB.
		let content = (0..200)
			.map(|i| format!("{i} {}", "y".repeat(300)))
			.collect::<Vec<_>>()
			.join("\n");
		std::fs::write(dir.join("f.txt"), content).unwrap();
		let result = run(&dir, json!({"pattern": "y{300}", "limit": 500})).await;
		let out = text(&result);
		assert!(out.contains("50.0KB limit reached"), "{out}");
	}

	#[tokio::test]
	async fn missing_path_errors() {
		let dir = tmp_dir("missing");
		let result = run(&dir, json!({"pattern": "x", "path": "no/such/dir"})).await;
		assert!(result.is_error);
		assert!(text(&result).starts_with("Path not found:"));
	}
}
