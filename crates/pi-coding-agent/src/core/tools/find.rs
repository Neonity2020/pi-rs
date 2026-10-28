//! `find` tool — port of `pi/packages/coding-agent/src/core/tools/find.ts`.
//!
//! Upstream shells out to fd (`fd --glob --color=never --hidden [--no-require-git]
//! --max-results N [--full-path] -- pattern path`); pi-rs walks with the
//! `ignore` crate (gitignore-aware, git-repo gated like fd's default) and
//! matches with the regex crate. **Accepted implementation difference**
//! (recorded in the P0 audit): fd subprocess vs library walker.
//!
//! fd semantics kept: patterns containing `/` match the whole path (with a
//! leading `**/` prepended unless the pattern already starts with `**/` or
//! `/`; a leading `/` matches the absolute path); other patterns match the
//! file name; directories are listed too; results are relativized against
//! the search root. Glob characters follow gitignore rules (`*` and `?` do
//! not cross `/`, `**` does).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};
use super::{param_str, param_str_or, param_u64_or, resolve_path};

/// `DEFAULT_LIMIT` upstream: default result cap.
const DEFAULT_LIMIT: usize = 1000;

pub struct FindTool {
	cwd: PathBuf,
}

impl FindTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

/// fd's gitignore-style glob → anchored regex: `**` crosses `/` (a `**/`
/// segment also matches zero directories), `*`/`?` do not cross `/`.
fn glob_to_regex(glob: &str) -> Result<Regex, String> {
	let mut regex = String::from("^");
	let mut chars = glob.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'*' => {
				if chars.peek() == Some(&'*') {
					chars.next();
					if chars.peek() == Some(&'/') {
						chars.next();
						regex.push_str("(?:.*/)?");
					} else {
						regex.push_str(".*");
					}
				} else {
					regex.push_str("[^/]*");
				}
			}
			'?' => regex.push_str("[^/]"),
			'.' => regex.push_str("\\."),
			'+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
				regex.push('\\');
				regex.push(c);
			}
			_ => regex.push(c),
		}
	}
	regex.push('$');
	Regex::new(&regex).map_err(|e| e.to_string())
}

/// True when `dir` or any ancestor holds a `.git` entry (fd's repo probe).
fn inside_git_repo(dir: &Path) -> bool {
	let mut current = Some(dir);
	while let Some(dir) = current {
		if dir.join(".git").exists() {
			return true;
		}
		current = dir.parent();
	}
	false
}

#[async_trait]
impl AgentTool for FindTool {
	fn name(&self) -> &str {
		"find"
	}

	fn description(&self) -> &str {
		"Search for files by glob pattern. Returns matching file paths relative \
to the search directory. Respects .gitignore. Output is truncated to 1000 \
results or 50KB (whichever is hit first)."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
				"path": { "type": "string", "description": "Directory to search in (default: current directory)" },
				"limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
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
		let pattern = match param_str(&params, "pattern") {
			Ok(p) => p,
			Err(e) => return *e,
		};
		let path = param_str_or(&params, "path", ".");
		let effective_limit = param_u64_or(&params, "limit", DEFAULT_LIMIT as u64).max(1) as usize;

		let search_path = resolve_path(&self.cwd, &path);
		if !search_path.exists() {
			return AgentToolResult::error(format!("Path not found: {}", search_path.display()));
		}

		// fd --glob matches the basename unless --full-path; in --full-path
		// mode a path-containing pattern needs a leading '**/' to match
		// anything (upstream prepends it, absolute patterns excepted).
		let full_path_mode = pattern.contains('/');
		let absolute_mode = pattern.starts_with('/');
		let effective_pattern = if full_path_mode && !absolute_mode {
			match pattern.starts_with("**/") || pattern == "**" {
				true => pattern.clone(),
				false => format!("**/{pattern}"),
			}
		} else {
			pattern.clone()
		};
		let regex = match glob_to_regex(&effective_pattern) {
			Ok(regex) => regex,
			Err(e) => return AgentToolResult::error(format!("invalid pattern: {e}")),
		};

		// fd ignores .gitignore outside git repos only with --no-require-git;
		// inside repos its default git-aware behavior keeps parent repo rules
		// from leaking across nested repo boundaries (upstream issue #5960).
		let in_repo = inside_git_repo(&search_path);
		let walker = ignore::WalkBuilder::new(&search_path)
			.hidden(false)
			.require_git(in_repo)
			.filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
			.build();

		let mut results: Vec<String> = Vec::new();
		let mut result_limit_reached = false;
		for entry in walker {
			if signal.is_cancelled() {
				return AgentToolResult::error("Operation aborted");
			}
			let Ok(entry) = entry else { continue };
			if results.len() >= effective_limit {
				result_limit_reached = true;
				break;
			}
			let path = entry.path();
			let candidate: String = if absolute_mode {
				path.to_string_lossy().replace('\\', "/")
			} else if full_path_mode {
				match path.strip_prefix(&search_path) {
					Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
					Err(_) => continue,
				}
			} else {
				match path.file_name() {
					Some(name) => name.to_string_lossy().into_owned(),
					None => continue,
				}
			};
			let matches = regex.is_match(&candidate);
			if !matches {
				continue;
			}
			// `relativizeFindResultPath`: results are relative to the search
			// root, posix separators, directories keep their trailing slash.
			let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
			let relative = path
				.strip_prefix(&search_path)
				.unwrap_or(path)
				.to_string_lossy()
				.replace('\\', "/");
			results.push(if is_dir && !relative.ends_with('/') {
				format!("{relative}/")
			} else {
				relative
			});
		}
		if results.len() >= effective_limit {
			result_limit_reached = true;
		}

		if results.is_empty() {
			return AgentToolResult::text("No files found matching pattern");
		}

		// Stable output: fd sorts entries within directories during its own
		// traversal; the library walker yields readdir order, so the capped
		// result set is sorted here.
		results.sort();

		let raw_output = results.join("\n");
		let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);
		let mut result_output = truncation.content;
		let mut notices: Vec<String> = Vec::new();
		if result_limit_reached {
			notices.push(format!(
				"{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
				effective_limit * 2
			));
		}
		if truncation.truncated {
			notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
		}
		if !notices.is_empty() {
			result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
		}

		let mut result = AgentToolResult::text(result_output);
		let mut details = json!({});
		if result_limit_reached {
			details["resultLimitReached"] = json!(effective_limit);
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
	use std::path::PathBuf;

	fn tmp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-find-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	async fn run(cwd: &Path, params: Value) -> AgentToolResult {
		FindTool::new(cwd.to_path_buf())
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

	fn sample_tree() -> PathBuf {
		let dir = tmp_dir("tree");
		std::fs::create_dir_all(dir.join("src/deep")).unwrap();
		std::fs::create_dir_all(dir.join("docs")).unwrap();
		std::fs::write(dir.join("main.rs"), "").unwrap();
		std::fs::write(dir.join("src/lib.rs"), "").unwrap();
		std::fs::write(dir.join("src/deep/util.rs"), "").unwrap();
		std::fs::write(dir.join("src/app.spec.ts"), "").unwrap();
		std::fs::write(dir.join("docs/readme.md"), "").unwrap();
		std::fs::write(dir.join(".gitignore"), "*.md\n").unwrap();
		dir
	}

	#[tokio::test]
	async fn basename_glob_matches_respects_gitignore() {
		let dir = sample_tree();
		let result = run(&dir, json!({"pattern": "*.rs"})).await;
		let out = text(&result);
		assert!(out.contains("main.rs"), "{out}");
		assert!(out.contains("src/lib.rs"), "{out}");
		assert!(out.contains("src/deep/util.rs"), "{out}");
		// gitignore keeps *.md out.
		let result = run(&dir, json!({"pattern": "*.md"})).await;
		assert_eq!(text(&result), "No files found matching pattern");
	}

	#[tokio::test]
	async fn full_path_pattern_with_directory_components() {
		let dir = sample_tree();
		// Path-containing patterns get a '**/' prefix and match the whole path.
		let result = run(&dir, json!({"pattern": "src/**/*.rs"})).await;
		let out = text(&result);
		assert!(out.contains("src/lib.rs"), "{out}");
		assert!(out.contains("src/deep/util.rs"), "{out}");
		assert!(!out.contains("main.rs"), "{out}");

		// Single-component path pattern.
		let result = run(&dir, json!({"pattern": "src/*.rs"})).await;
		let out = text(&result);
		assert!(out.contains("src/lib.rs"), "{out}");
		assert!(!out.contains("util.rs"), "{out}");

		// Absolute-style pattern (leading '/') matches the absolute path.
		let absolute_pattern = format!("{}/main.rs", dir.display());
		let result = run(&dir, json!({ "pattern": absolute_pattern })).await;
		let out = text(&result);
		assert!(out.contains("main.rs"), "{out}");
	}

	#[tokio::test]
	async fn directories_are_listed_with_trailing_slash() {
		let dir = sample_tree();
		let result = run(&dir, json!({"pattern": "docs"})).await;
		let out = text(&result);
		assert!(out.contains("docs/"), "{out}");
	}

	#[tokio::test]
	async fn limit_default_1000_and_notice() {
		let dir = tmp_dir("limit");
		for i in 0..1200 {
			std::fs::write(dir.join(format!("file{i:05}.txt")), "").unwrap();
		}
		let result = run(&dir, json!({"pattern": "*.txt"})).await;
		let out = text(&result);
		assert_eq!(out.lines().filter(|l| l.ends_with(".txt")).count(), 1000);
		assert!(
			out.contains("1000 results limit reached. Use limit=2000 for more, or refine pattern"),
			"{out}"
		);

		let result = run(&dir, json!({"pattern": "*.txt", "limit": 5})).await;
		let out = text(&result);
		assert_eq!(out.lines().filter(|l| l.ends_with(".txt")).count(), 5);
		assert!(out.contains("5 results limit reached"), "{out}");
	}

	#[tokio::test]
	async fn hidden_files_are_searched() {
		let dir = sample_tree();
		std::fs::write(dir.join(".hidden-glob.txt"), "").unwrap();
		let result = run(&dir, json!({"pattern": ".hidden*"})).await;
		assert!(text(&result).contains(".hidden-glob.txt"));
	}

	#[tokio::test]
	async fn gitignore_applies_outside_git_repos() {
		// fd gets --no-require-git outside repos: .gitignore still applies.
		let dir = tmp_dir("no-repo");
		std::fs::write(dir.join(".gitignore"), "skip.txt\n").unwrap();
		std::fs::write(dir.join("skip.txt"), "").unwrap();
		std::fs::write(dir.join("keep.txt"), "").unwrap();
		let result = run(&dir, json!({"pattern": "*.txt"})).await;
		let out = text(&result);
		assert!(out.contains("keep.txt"), "{out}");
		assert!(!out.contains("skip.txt"), "{out}");
	}

	#[tokio::test]
	async fn missing_path_errors() {
		let dir = tmp_dir("missing");
		let result = run(&dir, json!({"pattern": "*", "path": "no/such/dir"})).await;
		assert!(result.is_error);
		assert!(text(&result).starts_with("Path not found:"));
	}
}
