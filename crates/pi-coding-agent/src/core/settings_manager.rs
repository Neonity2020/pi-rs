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
	/// Raw `defaultTools` selection: plain names/patterns or `+name`/`-name`
	/// modifiers, resolved by `tool_selection::resolve_default_tools`.
	pub default_tools: Option<Vec<String>>,
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
