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
		Settings {
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
