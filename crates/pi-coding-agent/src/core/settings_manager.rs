//! Layered settings — port of `pi/packages/coding-agent/src/core/settings-manager.ts`
//! and `settings-defaults.ts`/`settings-schema.json`.
//!
//! Global `~/.pi-rs/agent/settings.json` merged with project `.pi/settings.json`
//! (project values win). Project files are honored once trust lands
//! (`project-trust.ts`); framework stage reads global only.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
	pub default_provider: Option<String>,
	pub default_model: Option<String>,
	pub default_thinking_level: Option<String>,
	pub theme: Option<String>,
	pub external_editor: Option<String>,
	pub shell_path: Option<String>,
	pub compaction: Option<CompactionSettings>,
	pub retry: Option<RetrySettings>,
	/// `branchSummary.skipPrompt` — skip the summarize-branch prompt when
	/// navigating the session tree (`/tree`).
	pub branch_summary: Option<BranchSummarySettings>,
	/// `treeFilterMode` — entry filter for the `/tree` selector: `default`,
	/// `no-tools`, `user-only`, `labeled-only`, or `all`.
	pub tree_filter_mode: Option<String>,
	/// `sessionDir` — session storage directory override (global settings;
	/// upstream `SettingsManager.getSessionDir`). CLI `--session-dir` and
	/// `PI_SESSION_DIR` take precedence.
	pub session_dir: Option<String>,
	/// Raw `defaultTools` selection: plain names/patterns or `+name`/`-name`
	/// modifiers, resolved by `tool_selection::resolve_default_tools`.
	pub default_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BranchSummarySettings {
	pub skip_prompt: Option<bool>,
}

impl BranchSummarySettings {
	fn merged(project: Option<&Self>, global: Option<&Self>) -> Self {
		Self {
			skip_prompt: project
				.and_then(|s| s.skip_prompt)
				.or_else(|| global.and_then(|s| s.skip_prompt)),
		}
	}
}

/// Upstream `getTreeFilterMode`: invalid values fall back to `"default"`.
pub fn normalize_tree_filter_mode(value: Option<&str>) -> &'static str {
	match value {
		Some("no-tools") => "no-tools",
		Some("user-only") => "user-only",
		Some("labeled-only") => "labeled-only",
		Some("all") => "all",
		_ => "default",
	}
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
	pub enabled: Option<bool>,
	pub max_retries: Option<u32>,
	pub base_delay_ms: Option<u64>,
	pub max_agent_delay_ms: Option<u64>,
}

impl RetrySettings {
	fn merged(project: Option<&Self>, global: Option<&Self>) -> Self {
		Self {
			enabled: project
				.and_then(|s| s.enabled)
				.or_else(|| global.and_then(|s| s.enabled)),
			max_retries: project
				.and_then(|s| s.max_retries)
				.or_else(|| global.and_then(|s| s.max_retries)),
			base_delay_ms: project
				.and_then(|s| s.base_delay_ms)
				.or_else(|| global.and_then(|s| s.base_delay_ms)),
			max_agent_delay_ms: project
				.and_then(|s| s.max_agent_delay_ms)
				.or_else(|| global.and_then(|s| s.max_agent_delay_ms)),
		}
	}
}

/// `compaction` settings — upstream `CompactionSettings`
/// (`compaction/compaction.ts` `DEFAULT_COMPACTION_SETTINGS`: enabled,
/// reserveTokens 16384, keepRecentTokens 20000; the call sites in
/// `agent_session.rs` fill those defaults for absent fields). Per-model
/// overrides (`compaction.modelOverrides`) are deferred.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
	pub enabled: Option<bool>,
	pub reserve_tokens: Option<u64>,
	pub keep_recent_tokens: Option<u64>,
}

impl CompactionSettings {
	fn merged(project: Option<&Self>, global: Option<&Self>) -> Self {
		Self {
			enabled: project
				.and_then(|s| s.enabled)
				.or_else(|| global.and_then(|s| s.enabled)),
			reserve_tokens: project
				.and_then(|s| s.reserve_tokens)
				.or_else(|| global.and_then(|s| s.reserve_tokens)),
			keep_recent_tokens: project
				.and_then(|s| s.keep_recent_tokens)
				.or_else(|| global.and_then(|s| s.keep_recent_tokens)),
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
	#[error("io error: {0}")]
	Io(#[from] std::io::Error),
	#[error("invalid settings JSON: {0}")]
	Json(#[from] serde_json::Error),
}

pub struct SettingsManager {
	global_path: PathBuf,
	project_path: Option<PathBuf>,
	global: Settings,
	project: Settings,
	/// Field keys modified in this session (for diagnostics/save prompts).
	#[allow(dead_code)]
	modified: Vec<String>,
}

impl SettingsManager {
	pub fn load(project_cwd: Option<&std::path::Path>) -> Self {
		let global_path = config::settings_json_path();
		let project_path = project_cwd.map(|cwd| config::project_pi_dir(cwd).join("settings.json"));

		let global = load_settings(&global_path).unwrap_or_default();
		let project = project_path
			.as_deref()
			.and_then(load_settings)
			.unwrap_or_default();

		Self {
			global_path,
			project_path,
			global,
			project,
			modified: Vec::new(),
		}
	}

	/// Effective settings: project over global, `None`s fall through.
	pub fn effective(&self) -> Settings {
		use crate::core::tool_selection::merge_default_tools;
		Settings {
			retry: Some(RetrySettings::merged(
				self.project.retry.as_ref(),
				self.global.retry.as_ref(),
			)),
			branch_summary: Some(BranchSummarySettings::merged(
				self.project.branch_summary.as_ref(),
				self.global.branch_summary.as_ref(),
			)),
			tree_filter_mode: self
				.project
				.tree_filter_mode
				.clone()
				.or_else(|| self.global.tree_filter_mode.clone()),
			session_dir: self
				.project
				.session_dir
				.clone()
				.or_else(|| self.global.session_dir.clone()),
			compaction: Some(CompactionSettings::merged(
				self.project.compaction.as_ref(),
				self.global.compaction.as_ref(),
			)),
			default_provider: self
				.project
				.default_provider
				.clone()
				.or(self.global.default_provider.clone()),
			default_model: self
				.project
				.default_model
				.clone()
				.or(self.global.default_model.clone()),
			default_thinking_level: self
				.project
				.default_thinking_level
				.clone()
				.or(self.global.default_thinking_level.clone()),
			theme: self.project.theme.clone().or(self.global.theme.clone()),
			external_editor: self
				.project
				.external_editor
				.clone()
				.or(self.global.external_editor.clone()),
			shell_path: self
				.project
				.shell_path
				.clone()
				.or(self.global.shell_path.clone()),
			default_tools: merge_default_tools(
				self.global.default_tools.clone(),
				self.project.default_tools.clone(),
			),
		}
	}

	pub fn global(&self) -> &Settings {
		&self.global
	}

	pub fn set_global(&mut self, settings: Settings) -> Result<(), SettingsError> {
		let dir = self.global_path.parent().unwrap_or(&self.global_path);
		std::fs::create_dir_all(dir)?;
		let json = serde_json::to_string_pretty(&settings)?;
		std::fs::write(&self.global_path, json)?;
		self.global = settings;
		Ok(())
	}
}

fn load_settings(path: &std::path::Path) -> Option<Settings> {
	let text = std::fs::read_to_string(path).ok()?;
	serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn retry_merges_fields_across_layers() {
		let global: Settings = serde_json::from_str(
			r#"{"retry":{"enabled":false,"maxRetries":5,"baseDelayMs":100,"maxAgentDelayMs":500}}"#,
		)
		.unwrap();
		let project: Settings =
			serde_json::from_str(r#"{"retry":{"enabled":true,"baseDelayMs":20}}"#).unwrap();
		let merged = RetrySettings::merged(project.retry.as_ref(), global.retry.as_ref());
		assert_eq!(
			(
				merged.enabled,
				merged.max_retries,
				merged.base_delay_ms,
				merged.max_agent_delay_ms
			),
			(Some(true), Some(5), Some(20), Some(500))
		);
	}
	#[test]
	fn compaction_merges_individual_settings() {
		let global: Settings = serde_json::from_str(
			r#"{"compaction":{"enabled":false,"reserveTokens":100,"keepRecentTokens":200}}"#,
		)
		.unwrap();
		let project: Settings =
			serde_json::from_str(r#"{"compaction":{"reserveTokens":50}}"#).unwrap();
		let merged =
			CompactionSettings::merged(project.compaction.as_ref(), global.compaction.as_ref());
		assert_eq!(
			(
				merged.enabled,
				merged.reserve_tokens,
				merged.keep_recent_tokens
			),
			(Some(false), Some(50), Some(200))
		);
	}

	#[test]
	fn default_tools_merge_across_settings_layers() {
		use crate::core::tool_selection::{merge_default_tools, resolve_default_tools};
		// Project modifier-only entries append to the global selection; plain
		// names replace it outright (upstream `mergeDefaultTools`).
		let global: Settings =
			serde_json::from_str(r#"{"defaultTools":["read","bash","edit","write"]}"#).unwrap();
		let project: Settings = serde_json::from_str(r#"{"defaultTools":["+grep"]}"#).unwrap();
		let merged =
			merge_default_tools(global.default_tools.clone(), project.default_tools.clone())
				.unwrap();
		assert_eq!(
			resolve_default_tools(&merged),
			vec!["read", "bash", "edit", "write", "grep"]
		);
		let plain: Settings = serde_json::from_str(r#"{"defaultTools":["bash","grep"]}"#).unwrap();
		let merged = merge_default_tools(global.default_tools, plain.default_tools).unwrap();
		assert_eq!(merged, vec!["bash", "grep"]);
	}
}

#[cfg(test)]
mod retry_settings_tests {
	use super::*;

	#[test]
	fn retry_settings_parse_and_fall_through() {
		let global: Settings =
			serde_json::from_str(r#"{"retry":{"enabled":false,"maxRetries":5,"baseDelayMs":100}}"#)
				.unwrap();
		let retry = global.retry.as_ref().expect("retry parsed");
		assert_eq!(retry.enabled, Some(false));
		assert_eq!(retry.max_retries, Some(5));
		assert_eq!(retry.base_delay_ms, Some(100));

		// Project overrides per field; missing fields fall through to global
		// (effective() merge, project wins).
		let project: Settings = serde_json::from_str(r#"{"retry":{"maxRetries":2}}"#).unwrap();
		let merged = RetrySettings::merged(project.retry.as_ref(), global.retry.as_ref());
		assert_eq!(merged.max_retries, Some(2));
		assert_eq!(merged.base_delay_ms, Some(100));

		// No retry key anywhere → None → engine defaults.
		let plain: Settings = serde_json::from_str("{}").unwrap();
		assert!(plain.retry.is_none());
	}
}
