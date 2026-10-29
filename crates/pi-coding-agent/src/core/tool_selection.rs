//! Tool selection — port of the upstream tool-list handling spread across
//! `core/settings-manager.ts` (`DEFAULT_TOOL_NAMES`, `isToolModifier`,
//! `getToolListError`, `applyToolModifiers`, `mergeDefaultTools`/
//! `resolveDefaultTools`, `getDefaultTools`), `core/mcp-servers.ts`
//! (`toolPatternRegExp`, `createToolNameMatcher`) and the CLI/SDK resolution
//! in `main.ts` + `core/sdk.ts` (`createAgentSession`'s
//! `initialActiveToolNames` block).
//!
//! Upstream shapes that matter here:
//! - The default active builtin set is `["read", "bash", "edit", "write"]`;
//!   `grep`/`find`/`ls` ship as builtin definitions but are only declared to
//!   the model when selected (`--tools +grep`, a plain-name list, or the
//!   `defaultTools` setting).
//! - A `--tools` list is either an allowlist of plain names/patterns (it
//!   replaces the default selection) or a list of only `+name`/`-name`
//!   modifiers (it edits the default selection). Mixing the two forms is an
//!   error; modifiers take exact names, not patterns.
//! - `-n` (`--no-tools`) empties the builtin set and also filters MCP tools;
//!   `-nbt` (`--no-builtin-tools`) empties only the builtins.
//! - MCP tools (`mcp__<server>__<tool>`) are filtered when the resolved
//!   allowlist is empty or contains `mcp__` entries (upstream
//!   `_allowlistFiltersMcp`); a plain builtin allowlist like `--tools bash`
//!   leaves MCP tools untouched.

/// Tools enabled at startup when neither the `defaultTools` setting nor
/// `--tools` changes them (`settings-manager.ts` `DEFAULT_TOOL_NAMES`).
pub const DEFAULT_TOOL_NAMES: &[&str] = &["read", "bash", "edit", "write"];

/// Whether a tool selection entry is a `+name` or `-name` modifier.
pub fn is_tool_modifier(entry: &str) -> bool {
	entry.starts_with('+') || entry.starts_with('-')
}

/// Validate a `--tools` list: either an allowlist of plain names/patterns or a
/// list of only `+name`/`-name` entries with exact names. Returns the problem,
/// or `None` when the list is valid (`settings-manager.ts` `getToolListError`).
pub fn get_tool_list_error(entries: &[String]) -> Option<String> {
	let modifiers: Vec<&String> = entries.iter().filter(|e| is_tool_modifier(e)).collect();
	if modifiers.is_empty() {
		return None;
	}
	if modifiers.len() < entries.len() {
		return Some("tool names cannot be mixed with +name or -name entries".into());
	}
	if let Some(pattern) = modifiers.iter().find(|e| e.contains('*')) {
		return Some(format!(
			"+name and -name entries take exact tool names, not patterns: {pattern}"
		));
	}
	None
}

/// Apply the `+name`/`-name` entries of `entries` to `base` in order: `+name`
/// adds a tool, `-name` removes one; other entries are ignored
/// (`settings-manager.ts` `applyToolModifiers`).
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
	let mut tools = base.to_vec();
	for entry in entries {
		if !is_tool_modifier(entry) {
			continue;
		}
		let name = &entry[1..];
		let index = tools.iter().position(|t| t == name);
		match (entry.starts_with('+'), index) {
			(true, None) if !name.is_empty() => tools.push(name.to_string()),
			(false, Some(index)) => {
				tools.remove(index);
			}
			_ => {}
		}
	}
	tools
}

/// Merge two `defaultTools` settings layers: a list with plain tool names
/// replaces the inherited one; a list of only `+name`/`-name` entries appends,
/// so it modifies the inherited selection (`mergeDefaultTools`).
pub fn merge_default_tools(
	base: Option<Vec<String>>,
	overrides: Option<Vec<String>>,
) -> Option<Vec<String>> {
	let overrides = overrides?;
	match base {
		Some(base) if overrides.iter().all(|e| is_tool_modifier(e)) => {
			let mut merged = base;
			merged.extend(overrides);
			Some(merged)
		}
		_ => Some(overrides),
	}
}

/// Resolve a merged `defaultTools` list: plain names replace the default set,
/// then `+name` adds and `-name` removes, in list order (`resolveDefaultTools`).
pub fn resolve_default_tools(entries: &[String]) -> Vec<String> {
	let plain: Vec<String> = entries
		.iter()
		.filter(|e| !is_tool_modifier(e))
		.cloned()
		.collect();
	let base: Vec<String> = if !plain.is_empty() || entries.is_empty() {
		plain
	} else {
		DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect()
	};
	apply_tool_modifiers(&base, entries)
}

/// `-nt` (`--no-tools`) vs `-nbt` (`--no-builtin-tools`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTools {
	/// No tools at all: builtins emptied and MCP bridges filtered out.
	All,
	/// Only builtin definitions emptied; MCP tools stay available.
	Builtin,
}

/// Exact-name-or-`*`-pattern tool name matcher (`mcp-servers.ts`
/// `createToolNameMatcher` + `toolPatternRegExp`: `*` becomes `.*`, every
/// other character is literal, anchored both ends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolMatcher {
	names: Vec<String>,
	patterns: Vec<String>,
}

impl ToolMatcher {
	pub fn new(entries: &[String]) -> Self {
		Self {
			names: entries
				.iter()
				.filter(|e| !e.contains('*'))
				.cloned()
				.collect(),
			patterns: entries
				.iter()
				.filter(|e| e.contains('*'))
				.cloned()
				.collect(),
		}
	}

	pub fn matches(&self, name: &str) -> bool {
		self.names.iter().any(|n| n == name)
			|| self.patterns.iter().any(|p| Self::pattern_matches(p, name))
	}

	fn pattern_matches(pattern: &str, text: &str) -> bool {
		let parts: Vec<&str> = pattern.split('*').collect();
		if parts.len() == 1 {
			return text == pattern;
		}
		let first = parts[0];
		let last = parts[parts.len() - 1];
		if text.len() < first.len() + last.len() {
			return false;
		}
		if !text.starts_with(first) || !text.ends_with(last) {
			return false;
		}
		let mut rest = &text[first.len()..text.len() - last.len()];
		for part in &parts[1..parts.len() - 1] {
			match rest.find(part) {
				Some(index) => rest = &rest[index + part.len()..],
				None => return false,
			}
		}
		true
	}
}

/// The resolved startup selection (`core/sdk.ts` `createAgentSession`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSelection {
	/// Builtin tool names declared to the model.
	pub builtin_active: Vec<String>,
	/// `Some(matcher)` when the allowlist reaches MCP tools (upstream
	/// `_allowlistFiltersMcp`); `None` leaves MCP tools untouched.
	pub mcp_filter: Option<ToolMatcher>,
}

/// Inputs to the startup resolution; `None`/empty means "flag not given".
#[derive(Debug, Clone, Default)]
pub struct ToolSelectionInput<'a> {
	/// `--tools` entries (already comma-split).
	pub tools: Option<&'a [String]>,
	/// `--exclude-tools` entries (already comma-split).
	pub exclude_tools: &'a [String],
	/// `-nt` / `-nbt`.
	pub no_tools: Option<NoTools>,
	/// The merged `defaultTools` settings value before resolution
	/// ([`resolve_default_tools`] runs inside, like upstream `getDefaultTools`).
	pub settings_default_tools: Option<&'a [String]>,
}

/// Errors of the startup resolution; upstream throws from
/// `createAgentSession` (a diagnostics error, exit code 1).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct ToolListError(pub String);

/// Port of the `createAgentSession` tool-resolution block: settings and flags
/// fold into the active builtin set plus an optional MCP allowlist.
pub fn resolve_tool_selection(
	input: ToolSelectionInput<'_>,
) -> Result<ToolSelection, ToolListError> {
	if let Some(error) = input.tools.and_then(get_tool_list_error) {
		return Err(ToolListError(format!("Invalid tools option: {error}")));
	}
	let default_tool_names: Vec<String> = if input.no_tools.is_some() {
		Vec::new()
	} else {
		input
			.settings_default_tools
			.map(resolve_default_tools)
			.unwrap_or_else(|| DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect())
	};
	let has_modifiers = input
		.tools
		.is_some_and(|tools| tools.iter().any(|e| is_tool_modifier(e)));
	let selected: Option<Vec<String>> = if has_modifiers {
		Some(apply_tool_modifiers(
			&default_tool_names,
			input.tools.unwrap_or_default(),
		))
	} else {
		input.tools.map(|tools| tools.to_vec())
	};
	let allowed: Option<Vec<String>> = if has_modifiers {
		if input.no_tools == Some(NoTools::All) {
			selected.clone()
		} else {
			None
		}
	} else {
		input.tools.map(|tools| tools.to_vec()).or_else(|| {
			if input.no_tools == Some(NoTools::All) {
				Some(Vec::new())
			} else {
				None
			}
		})
	};
	let exclude = (!input.exclude_tools.is_empty()).then(|| ToolMatcher::new(input.exclude_tools));
	let builtin_active = selected
		.unwrap_or_else(|| default_tool_names.clone())
		.into_iter()
		.filter(|name| !exclude.as_ref().is_some_and(|m| m.matches(name)))
		.collect();
	// `_allowlistFiltersMcp`: the allowlist reaches MCP tools only when it is
	// empty (no tools at all) or explicitly carries `mcp__` entries.
	let mcp_filter = allowed.and_then(|allowed| {
		let reaches_mcp =
			allowed.is_empty() || allowed.iter().any(|entry| entry.starts_with("mcp__"));
		reaches_mcp.then(|| ToolMatcher::new(&allowed))
	});
	Ok(ToolSelection {
		builtin_active,
		mcp_filter,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn list(entries: &[&str]) -> Vec<String> {
		entries.iter().map(|s| s.to_string()).collect()
	}

	#[test]
	fn default_tool_names_match_upstream() {
		assert_eq!(DEFAULT_TOOL_NAMES, &["read", "bash", "edit", "write"]);
	}

	#[test]
	fn tool_list_error_rejects_mixed_forms_and_modifier_patterns() {
		assert_eq!(get_tool_list_error(&list(&["bash", "grep"])), None);
		assert_eq!(
			get_tool_list_error(&list(&["bash", "+grep"])),
			Some("tool names cannot be mixed with +name or -name entries".into())
		);
		assert_eq!(
			get_tool_list_error(&list(&["+grep*", "-read"])),
			Some("+name and -name entries take exact tool names, not patterns: +grep*".into())
		);
		assert_eq!(get_tool_list_error(&list(&["+grep", "-read"])), None);
	}

	#[test]
	fn apply_tool_modifiers_edits_in_order() {
		let base = list(&["read", "bash", "edit", "write"]);
		assert_eq!(
			apply_tool_modifiers(&base, &list(&["+grep", "-bash"])),
			list(&["read", "edit", "write", "grep"])
		);
		// Removing an absent tool and adding an existing one are no-ops.
		assert_eq!(
			apply_tool_modifiers(&base, &list(&["+bash", "-find"])),
			base
		);
		// A bare "+" adds nothing (upstream `&& name`).
		assert_eq!(apply_tool_modifiers(&base, &list(&["+"])), base);
	}

	#[test]
	fn default_tools_settings_merge_and_resolve() {
		let base = list(&["read", "bash"]);
		// Plain names replace; modifier-only entries append (unresolved —
		// `mergeDefaultTools` concatenates, `resolveDefaultTools` applies).
		assert_eq!(
			merge_default_tools(Some(base.clone()), Some(list(&["edit", "write"]))),
			Some(list(&["edit", "write"]))
		);
		assert_eq!(
			merge_default_tools(Some(base.clone()), Some(list(&["+grep"]))),
			Some(list(&["read", "bash", "+grep"]))
		);
		assert_eq!(
			merge_default_tools(None, Some(list(&["+ls"]))),
			Some(list(&["+ls"]))
		);
		assert_eq!(merge_default_tools(Some(base), None), None);
		// Modifier-only resolved lists start from the upstream default set.
		assert_eq!(
			resolve_default_tools(&list(&["+grep", "-bash"])),
			list(&["read", "edit", "write", "grep"])
		);
		assert_eq!(resolve_default_tools(&list(&[])), Vec::<String>::new());
	}

	#[test]
	fn tool_matcher_supports_exact_and_star_patterns() {
		let matcher = ToolMatcher::new(&list(&["bash", "mcp__fs__*", "ed*t"]));
		assert!(matcher.matches("bash"));
		assert!(matcher.matches("mcp__fs__read"));
		assert!(matcher.matches("edit"));
		assert!(matcher.matches("edXt"), "`*` spans any run of characters");
		assert!(!matcher.matches("grep"));
		assert!(!matcher.matches("mcp__other__read"));
		// `*` is the only wildcard; every other character (including `?`) is
		// literal, matching upstream `toolPatternRegExp` escaping.
		assert!(ToolMatcher::new(&list(&["ed?t"])).matches("ed?t"));
		assert!(!ToolMatcher::new(&list(&["ed?t"])).matches("edit"));
	}

	#[test]
	fn resolution_defaults_to_four_builtin_tools() {
		let selection = resolve_tool_selection(ToolSelectionInput::default()).unwrap();
		assert_eq!(
			selection.builtin_active,
			list(&["read", "bash", "edit", "write"])
		);
		assert_eq!(selection.mcp_filter, None);
	}

	#[test]
	fn plain_tools_list_replaces_selection_and_leaves_mcp_alone() {
		let selection = resolve_tool_selection(ToolSelectionInput {
			tools: Some(&list(&["bash", "grep"])),
			..Default::default()
		})
		.unwrap();
		assert_eq!(selection.builtin_active, list(&["bash", "grep"]));
		assert_eq!(
			selection.mcp_filter, None,
			"upstream `_allowlistFiltersMcp` is false here"
		);
	}

	#[test]
	fn modifiers_edit_the_default_selection() {
		let selection = resolve_tool_selection(ToolSelectionInput {
			tools: Some(&list(&["+grep", "-write"])),
			..Default::default()
		})
		.unwrap();
		assert_eq!(
			selection.builtin_active,
			list(&["read", "bash", "edit", "grep"])
		);
		assert_eq!(selection.mcp_filter, None);
	}

	#[test]
	fn no_tools_filters_mcp_but_no_builtin_tools_does_not() {
		let all = resolve_tool_selection(ToolSelectionInput {
			no_tools: Some(NoTools::All),
			..Default::default()
		})
		.unwrap();
		assert!(all.builtin_active.is_empty());
		assert_eq!(all.mcp_filter, Some(ToolMatcher::new(&[])));

		let builtin = resolve_tool_selection(ToolSelectionInput {
			no_tools: Some(NoTools::Builtin),
			..Default::default()
		})
		.unwrap();
		assert!(builtin.builtin_active.is_empty());
		assert_eq!(builtin.mcp_filter, None);
	}

	#[test]
	fn exclude_tools_filter_the_selection() {
		let selection = resolve_tool_selection(ToolSelectionInput {
			exclude_tools: &list(&["bash", "mcp__fs__*"]),
			..Default::default()
		})
		.unwrap();
		assert_eq!(selection.builtin_active, list(&["read", "edit", "write"]));
		// The exclude matcher is applied per-name by the caller for MCP tools.
		assert!(ToolMatcher::new(&list(&["bash", "mcp__fs__*"])).matches("mcp__fs__read"));
	}

	#[test]
	fn invalid_tool_list_errors_like_upstream() {
		let error = resolve_tool_selection(ToolSelectionInput {
			tools: Some(&list(&["bash", "+grep"])),
			..Default::default()
		})
		.unwrap_err();
		assert_eq!(
			error.0,
			"Invalid tools option: tool names cannot be mixed with +name or -name entries"
		);
	}

	#[test]
	fn modifiers_over_no_tools_select_from_nothing() {
		// Upstream: `-nt -t +grep` → default [] with +grep applied. The
		// allowlist equals the selection, but a non-empty list without
		// `mcp__` entries does not reach MCP (`_allowlistFiltersMcp` false).
		let selection = resolve_tool_selection(ToolSelectionInput {
			tools: Some(&list(&["+grep"])),
			no_tools: Some(NoTools::All),
			..Default::default()
		})
		.unwrap();
		assert_eq!(selection.builtin_active, list(&["grep"]));
		assert_eq!(selection.mcp_filter, None);
	}
}
