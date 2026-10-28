//! `edit` tool — port of `pi/packages/coding-agent/src/core/tools/edit.ts`.
//!
//! Wire schema is upstream's: `path` + `edits: [{oldText, newText}]` (one
//! call may carry several disjoint replacements). `prepare_edit_arguments`
//! copies upstream's `prepareEditArguments` leniency: models send `edits`
//! as a JSON string, as a single edit object, or use the legacy flat
//! `oldText`/`newText` pair — all are normalized to the array form.
//!
//! Accepted differences vs upstream (recorded, not blocking):
//! - No fuzzy-match fallback for non-exact `oldText` (see tools/edit_diff.rs).
//! - No file-mutation-queue serialization (the extension-host machinery
//!   lands with the P2 extension system); pi-rs tools run sequentially.
//! - The abort signal is observed between the file operations, not from an
//!   event listener.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};

use super::edit_diff::{
	Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
	generate_unified_patch, normalize_to_lf, restore_line_endings, split_bom,
};
use super::{param_str, resolve_path};

pub struct EditTool {
	cwd: PathBuf,
}

impl EditTool {
	pub fn new(cwd: PathBuf) -> Self {
		Self { cwd }
	}
}

/// `isSingleEditInput` upstream: a bare `{oldText, newText}` object.
fn is_single_edit_input(value: &Value) -> bool {
	value.as_object().is_some_and(|edit| {
		edit.get("oldText").is_some_and(Value::is_string)
			&& edit.get("newText").is_some_and(Value::is_string)
	})
}

/// `prepareEditArguments` upstream: normalize the shapes models actually
/// send into the canonical `{path, edits: [{oldText, newText}]}` form.
fn prepare_edit_arguments(params: &Value) -> Value {
	let mut args = params.clone();
	let Some(obj) = args.as_object_mut() else {
		return args;
	};

	// Some models send edits as a JSON string instead of an array.
	// Others send a single edit object instead of a one-element array.
	if let Some(edits) = obj.get("edits").cloned() {
		if let Some(text) = edits.as_str() {
			if let Ok(parsed) = serde_json::from_str::<Value>(text) {
				if parsed.is_array() {
					obj.insert("edits".into(), parsed);
				} else if is_single_edit_input(&parsed) {
					obj.insert("edits".into(), json!([parsed]));
				}
			}
		} else if is_single_edit_input(&edits) {
			obj.insert("edits".into(), json!([edits]));
		}
	}

	// Legacy flat form: {oldText, newText} at the top level.
	let old_text = obj.get("oldText").and_then(Value::as_str);
	let new_text = obj.get("newText").and_then(Value::as_str);
	if let (Some(old_text), Some(new_text)) = (old_text, new_text) {
		let mut edits: Vec<Value> = obj
			.get("edits")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		edits.push(json!({ "oldText": old_text, "newText": new_text }));
		obj.remove("oldText");
		obj.remove("newText");
		obj.insert("edits".into(), Value::Array(edits));
	}
	args
}

/// Parse the canonical `edits` array; `None` marks an invalid input.
fn parse_edits(edits: &Value) -> Option<Vec<Edit>> {
	let arr = edits.as_array()?;
	let mut parsed = Vec::with_capacity(arr.len());
	for entry in arr {
		let obj = entry.as_object()?;
		let old_text = obj.get("oldText")?.as_str()?.to_string();
		let new_text = obj.get("newText")?.as_str()?.to_string();
		parsed.push(Edit { old_text, new_text });
	}
	Some(parsed)
}

#[async_trait]
impl AgentTool for EditTool {
	fn name(&self) -> &str {
		"edit"
	}

	fn description(&self) -> &str {
		"Edit a single file using exact text replacement. Every edits[].oldText \
must match a unique, non-overlapping region of the original file. If two \
changes affect the same block or nearby lines, merge them into one edit \
instead of emitting overlapping edits. Do not include large unchanged \
regions just to connect distant changes."
	}

	fn parameters_schema(&self) -> Value {
		json!({
			"type": "object",
			"properties": {
				"path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
				"edits": {
					"type": "array",
					"description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
					"items": {
						"type": "object",
						"properties": {
							"oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
							"newText": { "type": "string", "description": "Replacement text for this targeted edit." }
						},
						"required": ["oldText", "newText"]
					}
				}
			},
			"required": ["path", "edits"]
		})
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let prepared = prepare_edit_arguments(&params);

		let path = match param_str(&prepared, "path") {
			Ok(p) => p,
			Err(e) => return *e,
		};
		let edits = match prepared.get("edits").and_then(parse_edits) {
			Some(edits) if !edits.is_empty() => edits,
			_ => {
				return AgentToolResult::error(
					"Edit tool input is invalid. edits must contain at least one replacement.",
				);
			}
		};

		if signal.is_cancelled() {
			return AgentToolResult::error("Operation aborted");
		}

		let resolved = resolve_path(&self.cwd, &path);
		// Read as bytes decoded lossily, like the TS `buffer.toString("utf-8")`.
		let raw_content = match tokio::fs::read(&resolved).await {
			Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
			Err(e) => return AgentToolResult::error(format!("Could not edit file: {path}. {e}.")),
		};
		if signal.is_cancelled() {
			return AgentToolResult::error("Operation aborted");
		}

		// Strip the BOM before matching (the model never includes it in
		// oldText), normalize CRLF to LF, apply, then restore both on write.
		let (bom, text) = split_bom(&raw_content);
		let original_ending = detect_line_ending(text);
		let normalized_content = normalize_to_lf(text);
		let (base_content, new_content) =
			match apply_edits_to_normalized_content(&normalized_content, &edits, &path) {
				Ok(result) => result,
				Err(message) => return AgentToolResult::error(message),
			};
		if signal.is_cancelled() {
			return AgentToolResult::error("Operation aborted");
		}

		let final_content = format!(
			"{bom}{}",
			restore_line_endings(&new_content, original_ending)
		);
		if let Err(e) = tokio::fs::write(&resolved, final_content).await {
			return AgentToolResult::error(format!("Could not edit file: {path}. {e}."));
		}

		let (diff, first_changed_line) = generate_diff_string(&base_content, &new_content, 4);
		let patch = generate_unified_patch(&path, &base_content, &new_content, 4);
		let mut result = AgentToolResult::text(format!(
			"Successfully replaced {} block(s) in {path}.",
			edits.len()
		));
		result.details = Some(json!({
			"diff": diff,
			"patch": patch,
			"firstChangedLine": first_changed_line,
		}));
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::Path;

	fn tmp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-edit-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	async fn run_edit(cwd: &Path, params: Value) -> AgentToolResult {
		EditTool::new(cwd.to_path_buf())
			.execute("test", params, CancellationToken::new(), None)
			.await
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
	async fn applies_array_of_edits_and_reports_diff_details() {
		let dir = tmp_dir("array");
		let file = dir.join("f.txt");
		std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": [
					{ "oldText": "alpha", "newText": "ALPHA" },
					{ "oldText": "gamma", "newText": "GAMMA" }
				]
			}),
		)
		.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert_eq!(
			result_text(&result),
			"Successfully replaced 2 block(s) in f.txt."
		);
		assert_eq!(
			std::fs::read_to_string(&file).unwrap(),
			"ALPHA\nbeta\nGAMMA\n"
		);

		let details = result.details.expect("details carry diff/patch");
		let diff = details["diff"].as_str().unwrap();
		assert!(
			diff.lines()
				.any(|line| line.starts_with('+') && line.ends_with("ALPHA")),
			"{diff}"
		);
		assert!(
			diff.lines()
				.any(|line| line.starts_with('+') && line.ends_with("GAMMA")),
			"{diff}"
		);
		assert!(details["patch"].as_str().unwrap().starts_with("--- f.txt"));
		assert_eq!(details["firstChangedLine"].as_u64(), Some(1));
	}

	#[tokio::test]
	async fn accepts_edits_as_json_string() {
		let dir = tmp_dir("json-string");
		std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": "[{\"oldText\":\"hello\",\"newText\":\"hi\"}]"
			}),
		)
		.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "hi\n");
	}

	#[tokio::test]
	async fn accepts_single_edit_object_instead_of_array() {
		let dir = tmp_dir("single-object");
		std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": { "oldText": "hello", "newText": "hi" }
			}),
		)
		.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "hi\n");
	}

	#[tokio::test]
	async fn accepts_legacy_flat_oldtext_newtext_form() {
		let dir = tmp_dir("legacy");
		std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"oldText": "hello",
				"newText": "hi"
			}),
		)
		.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "hi\n");

		// Legacy pair alongside an edits array appends the replacement.
		std::fs::write(dir.join("g.txt"), "aaa bbb\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "g.txt",
				"edits": [{ "oldText": "aaa", "newText": "AAA" }],
				"oldText": "bbb",
				"newText": "BBB"
			}),
		)
		.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert_eq!(
			result_text(&result),
			"Successfully replaced 2 block(s) in g.txt."
		);
		assert_eq!(
			std::fs::read_to_string(dir.join("g.txt")).unwrap(),
			"AAA BBB\n"
		);
	}

	#[tokio::test]
	async fn preserves_bom_and_crlf_line_endings() {
		let dir = tmp_dir("bom-crlf");
		let file = dir.join("win.txt");
		std::fs::write(&file, "\u{feff}one\r\ntwo\r\n").unwrap();

		let result = run_edit(
			&dir,
			json!({
				"path": "win.txt",
				"edits": [{ "oldText": "one\ntwo", "newText": "one\nTWO" }]
			}),
		)
		.await;
		assert!(!result.is_error, "{}", result_text(&result));
		assert_eq!(std::fs::read(&file).unwrap(), b"\xef\xbb\xbfone\r\nTWO\r\n");
	}

	#[tokio::test]
	async fn duplicate_oldtext_fails_without_rewriting() {
		let dir = tmp_dir("duplicate");
		let file = dir.join("f.txt");
		std::fs::write(&file, "same same\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": [{ "oldText": "same", "newText": "different" }]
			}),
		)
		.await;
		assert!(result.is_error);
		assert!(
			result_text(&result).starts_with("Found 2 occurrences of the text in f.txt."),
			"{}",
			result_text(&result)
		);
		assert_eq!(std::fs::read_to_string(&file).unwrap(), "same same\n");
	}

	#[tokio::test]
	async fn overlapping_edits_fail() {
		let dir = tmp_dir("overlap");
		std::fs::write(dir.join("f.txt"), "one two\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": [
					{ "oldText": "one t", "newText": "X" },
					{ "oldText": "two", "newText": "Y" }
				]
			}),
		)
		.await;
		assert!(result.is_error);
		assert!(result_text(&result).contains("edits[0] and edits[1] overlap in f.txt"));
	}

	#[tokio::test]
	async fn missing_text_empty_edits_and_missing_file_fail() {
		let dir = tmp_dir("missing");
		std::fs::write(dir.join("f.txt"), "abc\n").unwrap();

		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": [{ "oldText": "zzz", "newText": "y" }]
			}),
		)
		.await;
		assert!(result.is_error);
		assert!(
			result_text(&result).starts_with(
				"Could not find the exact text in f.txt. The old text must match exactly"
			),
			"{}",
			result_text(&result)
		);

		// Several edits name the failing index.
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": [
					{ "oldText": "abc", "newText": "abd" },
					{ "oldText": "zzz", "newText": "y" }
				]
			}),
		)
		.await;
		assert!(result_text(&result).starts_with("Could not find edits[1] in f.txt."));

		let result = run_edit(&dir, json!({ "path": "f.txt", "edits": [] })).await;
		assert!(result.is_error);
		assert_eq!(
			result_text(&result),
			"Edit tool input is invalid. edits must contain at least one replacement."
		);

		let result = run_edit(
			&dir,
			json!({ "path": "nope.txt", "edits": [{ "oldText": "a", "newText": "b" }] }),
		)
		.await;
		assert!(result.is_error);
		assert!(result_text(&result).starts_with("Could not edit file: nope.txt."));
	}

	#[tokio::test]
	async fn no_change_edit_fails() {
		let dir = tmp_dir("noop");
		std::fs::write(dir.join("f.txt"), "abc\n").unwrap();
		let result = run_edit(
			&dir,
			json!({
				"path": "f.txt",
				"edits": [{ "oldText": "abc", "newText": "abc" }]
			}),
		)
		.await;
		assert!(result.is_error);
		assert!(result_text(&result).starts_with("No changes made to f.txt."));
	}
}
