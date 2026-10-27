//! UI configuration loading — port of the user-config half of
//! `pi/packages/coding-agent/src/core/keybindings.ts` (keybindings) and the
//! theme-selection slice of
//! `pi/packages/coding-agent/src/modes/interactive/theme/theme.ts`
//! (+ `docs/themes.md`).
//!
//! Upstream file layout, matched here:
//! - `<agentDir>/keybindings.json` — user keybindings,
//!   `{"app.tools.expand": ["ctrl+o", "f2"], ...}` (a key id or a list of
//!   them per action, `KeybindingValueSchema`).
//! - `<agentDir>/themes/<name>.json` — custom themes; there is no single
//!   `theme.json`. The theme is chosen by the settings.json `theme` field
//!   (a theme name); `dark`/`light` name the built-ins.
//!
//! Failure behavior (per task spec, diverging from upstream where noted):
//! - keybindings: missing file → defaults silently (upstream parity); bad
//!   JSON / unreadable file / unparseable chord → a notice and defaults
//!   (upstream silently ignores; pi-rs surfaces the error).
//! - theme: missing/invalid theme file → a notice and the built-in
//!   `dark`/`light` theme chosen by `--light` (upstream falls back to its
//!   generated `system` theme, which pi-rs does not implement).
//!
//! Notices are returned in [`UiConfig::notices`] and surfaced through the
//! interactive mode's notice channel; print/rpc render no UI, so nothing
//! else consumes them.

use std::collections::HashMap;
use std::path::Path;

use pi_tui::keybindings::{KeybindingDefinition, KeybindingDefinitions};
use pi_tui::{KeybindingsManager, TUI_KEYBINDINGS, Theme};

use crate::config;

/// Platform-picked defaults (upstream computes these at module load from
/// `process.platform`). Windows/WSL variants: upstream picks `alt+*`/`ctrl+q`
/// shapes on win32.
#[cfg(target_os = "windows")]
const MODEL_CYCLE_BACKWARD: &[&str] = &["alt+p"];
#[cfg(not(target_os = "windows"))]
const MODEL_CYCLE_BACKWARD: &[&str] = &["shift+ctrl+p"];
#[cfg(target_os = "windows")]
const MESSAGE_FOLLOW_UP: &[&str] = &["ctrl+q"];
#[cfg(not(target_os = "windows"))]
const MESSAGE_FOLLOW_UP: &[&str] = &["alt+enter"];
#[cfg(target_os = "windows")]
const MESSAGE_DEQUEUE: &[&str] = &["alt+q"];
#[cfg(not(target_os = "windows"))]
const MESSAGE_DEQUEUE: &[&str] = &["alt+up"];
#[cfg(target_os = "windows")]
const CLIPBOARD_PASTE_IMAGE: &[&str] = &["alt+v"];
#[cfg(not(target_os = "windows"))]
const CLIPBOARD_PASTE_IMAGE: &[&str] = &["ctrl+v"];
/// Windows has no suspend chord (upstream: empty default on win32).
#[cfg(target_os = "windows")]
const APP_SUSPEND: &[&str] = &[];
#[cfg(not(target_os = "windows"))]
const APP_SUSPEND: &[&str] = &["ctrl+z"];
/// macOS orders the alt chord first (upstream `process.platform === "darwin"`).
#[cfg(target_os = "macos")]
const TREE_FOLD_OR_UP: &[&str] = &["alt+left", "ctrl+left"];
#[cfg(not(target_os = "macos"))]
const TREE_FOLD_OR_UP: &[&str] = &["ctrl+left", "alt+left"];
#[cfg(target_os = "macos")]
const TREE_UNFOLD_OR_DOWN: &[&str] = &["alt+right", "ctrl+right"];
#[cfg(not(target_os = "macos"))]
const TREE_UNFOLD_OR_DOWN: &[&str] = &["ctrl+right", "alt+right"];

const fn def(
	default_keys: &'static [&'static str],
	description: &'static str,
) -> KeybindingDefinition {
	KeybindingDefinition {
		default_keys,
		description,
	}
}

/// App-level action definitions — upstream `APP_KEYBINDINGS` in
/// `core/keybindings.ts`, transcribed verbatim (defaults, descriptions,
/// order). pi-rs interactive dispatches only the actions it implements;
/// the rest stay here so keybindings.json can already bind them.
pub const APP_KEYBINDINGS: KeybindingDefinitions = &[
	("app.interrupt", def(&["escape"], "Cancel or abort")),
	("app.clear", def(&["ctrl+c"], "Clear editor")),
	("app.exit", def(&["ctrl+d"], "Exit when editor is empty")),
	("app.suspend", def(APP_SUSPEND, "Suspend to background")),
	(
		"app.thinking.cycle",
		def(&["shift+tab"], "Cycle thinking level"),
	),
	("app.thinking.save", def(&["ctrl+s"], "Save thinking level")),
	(
		"app.model.cycleForward",
		def(&["ctrl+p"], "Cycle to next model"),
	),
	(
		"app.model.cycleBackward",
		def(MODEL_CYCLE_BACKWARD, "Cycle to previous model"),
	),
	("app.model.select", def(&["ctrl+l"], "Open model selector")),
	("app.tools.expand", def(&["ctrl+o"], "Toggle tool output")),
	(
		"app.thinking.toggle",
		def(&["ctrl+t"], "Toggle thinking blocks"),
	),
	(
		"app.session.toggleNamedFilter",
		def(&["ctrl+n"], "Toggle named session filter"),
	),
	(
		"app.editor.external",
		def(&["ctrl+g"], "Open external editor"),
	),
	(
		"app.message.copy",
		def(&["ctrl+x"], "Copy selection or last assistant message"),
	),
	(
		"app.message.followUp",
		def(MESSAGE_FOLLOW_UP, "Queue follow-up message"),
	),
	(
		"app.message.dequeue",
		def(MESSAGE_DEQUEUE, "Restore queued messages"),
	),
	(
		"app.clipboard.pasteImage",
		def(
			CLIPBOARD_PASTE_IMAGE,
			"Paste files on macOS, images, or text from clipboard",
		),
	),
	("app.session.new", def(&[], "Start a new session")),
	("app.session.tree", def(&[], "Open session tree")),
	("app.session.fork", def(&[], "Fork current session")),
	("app.session.resume", def(&[], "Resume a session")),
	(
		"app.tree.foldOrUp",
		def(TREE_FOLD_OR_UP, "Fold tree branch or move up"),
	),
	(
		"app.tree.unfoldOrDown",
		def(TREE_UNFOLD_OR_DOWN, "Unfold tree branch or move down"),
	),
	("app.tree.editLabel", def(&["shift+l"], "Edit tree label")),
	(
		"app.tree.toggleLabelTimestamp",
		def(&["shift+t"], "Toggle tree label timestamps"),
	),
	(
		"app.session.togglePath",
		def(&["ctrl+p"], "Toggle session path display"),
	),
	(
		"app.session.toggleSort",
		def(&["ctrl+s"], "Toggle session sort mode"),
	),
	("app.session.rename", def(&["ctrl+r"], "Rename session")),
	("app.session.delete", def(&["ctrl+d"], "Delete session")),
	(
		"app.session.deleteNoninvasive",
		def(&["ctrl+backspace"], "Delete session when query is empty"),
	),
	("app.models.save", def(&["ctrl+s"], "Save model selection")),
	(
		"app.models.enableAll",
		def(&["ctrl+a"], "Enable all models"),
	),
	("app.models.clearAll", def(&["ctrl+x"], "Clear all models")),
	(
		"app.models.toggleProvider",
		def(&["ctrl+p"], "Toggle all models for provider"),
	),
	(
		"app.models.reorderUp",
		def(&["alt+up"], "Move model up in order"),
	),
	(
		"app.models.reorderDown",
		def(&["alt+down"], "Move model down in order"),
	),
	(
		"app.tree.filter.default",
		def(&["ctrl+d"], "Tree filter: default view"),
	),
	(
		"app.tree.filter.noTools",
		def(&["ctrl+t"], "Tree filter: hide tool results"),
	),
	(
		"app.tree.filter.userOnly",
		def(&["ctrl+u"], "Tree filter: user messages only"),
	),
	(
		"app.tree.filter.labeledOnly",
		def(&["ctrl+l"], "Tree filter: labeled entries only"),
	),
	(
		"app.tree.filter.all",
		def(&["ctrl+a"], "Tree filter: show all entries"),
	),
	(
		"app.tree.filter.cycleForward",
		def(&["ctrl+o"], "Tree filter: cycle forward"),
	),
	(
		"app.tree.filter.cycleBackward",
		def(&["shift+ctrl+o"], "Tree filter: cycle backward"),
	),
];

/// The manager's definition table: TUI actions first, then app actions —
/// upstream `KEYBINDINGS = { ...TUI_KEYBINDINGS, ...APP_KEYBINDINGS }`.
pub fn new_keybindings_manager() -> KeybindingsManager {
	KeybindingsManager::with_merged_definitions(TUI_KEYBINDINGS, APP_KEYBINDINGS)
}

/// Loaded UI configuration for the interactive mode.
pub struct UiConfig {
	/// The theme to render with (user theme or built-in fallback).
	pub theme: Theme,
	/// Keybindings manager over the merged TUI + app definition table with
	/// user overrides applied.
	pub keybindings: KeybindingsManager,
	/// Config problems to show the user at startup (bad keybindings.json,
	/// missing/broken theme file).
	pub notices: Vec<String>,
}

/// Load from the agent dir: `themes/<name>.json` for the theme named by
/// settings.json `theme`, and `keybindings.json` for user keybindings.
/// `light` selects the built-in fallback theme when no user theme applies
/// (pi-rs has no `system` theme; upstream's default).
pub fn load(theme_setting: Option<&str>, light: bool) -> UiConfig {
	let agent_dir = config::agent_dir();
	load_from(
		&agent_dir.join("keybindings.json"),
		&config::themes_dir(),
		theme_setting,
		light,
	)
}

/// Path-parameterized core of [`load`] so tests can point at a temp dir.
fn load_from(
	keybindings_path: &Path,
	themes_dir: &Path,
	theme_setting: Option<&str>,
	light: bool,
) -> UiConfig {
	let (overrides, mut notices) = load_keybinding_overrides(keybindings_path);
	let mut keybindings = new_keybindings_manager();
	if let Err(error) = keybindings.set_user_bindings(overrides) {
		// `set_user_bindings` is atomic: the manager keeps pure defaults.
		notices.push(format!(
			"keybindings.json: {error}; using default keybindings"
		));
	}
	let theme = load_theme(themes_dir, theme_setting, light, &mut notices);
	UiConfig {
		theme,
		keybindings,
		notices,
	}
}

/// Read user keybinding overrides (upstream `KeybindingsManager.loadFromFile`
/// plus `toKeybindingsConfig`). Missing file → no overrides, no notice
/// (upstream parity). Every other problem becomes a notice.
fn load_keybinding_overrides(path: &Path) -> (HashMap<String, Vec<String>>, Vec<String>) {
	let mut notices = Vec::new();
	if !path.exists() {
		return (HashMap::new(), notices);
	}
	let content = match std::fs::read_to_string(path) {
		Ok(content) => content,
		Err(error) => {
			notices.push(format!(
				"keybindings.json: {error}; using default keybindings"
			));
			return (HashMap::new(), notices);
		}
	};
	// Upstream `loadRawConfig`: stripBom, parse, require an object.
	let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
	let parsed: serde_json::Value = match serde_json::from_str(content) {
		Ok(value) => value,
		Err(error) => {
			notices.push(format!(
				"keybindings.json: failed to parse: {error}; using default keybindings"
			));
			return (HashMap::new(), notices);
		}
	};
	let Some(object) = parsed.as_object() else {
		notices.push(
			"keybindings.json: expected a JSON object; using default keybindings".to_string(),
		);
		return (HashMap::new(), notices);
	};

	// Upstream `toKeybindingsConfig`: values are a key id or a list of them;
	// schema-invalid values are dropped silently. Upstream also migrates
	// legacy action names (`expandTools` → `app.tools.expand`) — pi-rs has
	// no legacy users, so names pass through as-is.
	let mut overrides = HashMap::new();
	for (action, value) in object {
		if action == "$schema" {
			continue;
		}
		match value {
			serde_json::Value::String(key) => {
				overrides.insert(action.clone(), vec![key.clone()]);
			}
			serde_json::Value::Array(keys) => {
				let Some(keys) = keys
					.iter()
					.map(|key| key.as_str().map(str::to_string))
					.collect::<Option<Vec<_>>>()
				else {
					continue;
				};
				overrides.insert(action.clone(), keys);
			}
			_ => {}
		}
	}
	(overrides, notices)
}

/// Resolve the theme (upstream `initTheme`/`loadTheme` plus the settings
/// `theme` name): built-in `dark`/`light`, or `<themes_dir>/<name>.json`.
/// Any failure notices and falls back to the built-in theme chosen by
/// `light`.
fn load_theme(
	themes_dir: &Path,
	theme_setting: Option<&str>,
	light: bool,
	notices: &mut Vec<String>,
) -> Theme {
	let fallback = || {
		if light { Theme::light() } else { Theme::dark() }
	};
	let Some(name) = theme_setting else {
		return fallback();
	};
	// Built-in names shadow custom themes of the same name (upstream
	// `loadThemeJson` checks built-ins first). pi-rs has no `system` theme,
	// so that name falls through to the fallback below.
	if name == "dark" {
		return Theme::dark();
	}
	if name == "light" {
		return Theme::light();
	}
	// "/" is reserved for upstream's automatic light/dark setting format,
	// which pi-rs does not implement.
	if name.contains('/') || name.is_empty() {
		notices.push(format!(
			"theme \"{name}\" is not a usable theme name; using built-in {} theme",
			if light { "light" } else { "dark" }
		));
		return fallback();
	}
	let path = themes_dir.join(format!("{name}.json"));
	let content = match std::fs::read_to_string(&path) {
		Ok(content) => content,
		Err(error) => {
			notices.push(format!(
				"theme \"{name}\" ({}): {error}; using built-in {} theme",
				path.display(),
				if light { "light" } else { "dark" }
			));
			return fallback();
		}
	};
	match Theme::from_json_str(&content) {
		Ok(theme) => theme,
		Err(error) => {
			notices.push(format!(
				"theme \"{name}\" ({}): {error}; using built-in {} theme",
				path.display(),
				if light { "light" } else { "dark" }
			));
			fallback()
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_tui::Theme;
	use ratatui::style::Color;

	/// A fresh temp dir per call (process id + timestamp, like the other
	/// crates' tests) — tests pass explicit paths to [`load_from`], so no
	/// env-var redirection is needed.
	fn temp_agent_dir(tag: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-ui-config-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	/// `load_from` over a temp agent dir; returns the config and keeps the
	/// dir alive in the guard.
	fn load_in(
		tag: &str,
		theme_setting: Option<&str>,
		light: bool,
	) -> (UiConfig, std::path::PathBuf) {
		let dir = temp_agent_dir(tag);
		let ui = load_from(
			&dir.join("keybindings.json"),
			&dir.join("themes"),
			theme_setting,
			light,
		);
		(ui, dir)
	}

	#[test]
	fn app_keybindings_match_upstream_table() {
		assert_eq!(APP_KEYBINDINGS.len(), 43);
		let by_id: HashMap<&str, &KeybindingDefinition> =
			APP_KEYBINDINGS.iter().map(|(id, def)| (*id, def)).collect();
		assert_eq!(by_id["app.interrupt"].default_keys, &["escape"]);
		assert_eq!(by_id["app.clear"].default_keys, &["ctrl+c"]);
		assert_eq!(by_id["app.exit"].default_keys, &["ctrl+d"]);
		assert_eq!(by_id["app.model.select"].default_keys, &["ctrl+l"]);
		assert_eq!(by_id["app.tools.expand"].default_keys, &["ctrl+o"]);
		assert_eq!(by_id["app.thinking.toggle"].default_keys, &["ctrl+t"]);
		// Empty-default actions are preserved (session management actions
		// pi-rs does not implement yet).
		assert_eq!(by_id["app.session.new"].default_keys, &[] as &[&str]);
		assert_eq!(by_id["app.session.tree"].default_keys, &[] as &[&str]);
		// Every default chord parses, like the TUI table's invariant.
		for (id, definition) in APP_KEYBINDINGS {
			for key in definition.default_keys {
				assert!(pi_tui::parse_chord(key).is_ok(), "{id}: {key}");
			}
		}
	}

	#[test]
	fn merged_manager_covers_tui_and_app_actions() {
		let manager = new_keybindings_manager();
		assert_eq!(
			manager.resolved_bindings().len(),
			TUI_KEYBINDINGS.len() + APP_KEYBINDINGS.len()
		);
		assert!(manager.matches(
			"app.tools.expand",
			&pi_tui::parse_chord("ctrl+o").unwrap().to_key_event()
		));
		assert!(manager.matches(
			"tui.input.submit",
			&pi_tui::parse_chord("enter").unwrap().to_key_event()
		));
	}

	#[test]
	fn keybindings_missing_file_keeps_defaults() {
		let (ui, dir) = load_in("kb-missing", None, false);
		assert!(ui.notices.is_empty());
		assert!(ui.keybindings.matches(
			"app.tools.expand",
			&pi_tui::parse_chord("ctrl+o").unwrap().to_key_event()
		));
		assert!(!dir.join("keybindings.json").exists());
	}

	#[test]
	fn keybindings_overrides_apply_and_replace_defaults() {
		let (_, dir) = load_in("kb-good", None, false);
		std::fs::write(
			dir.join("keybindings.json"),
			r#"{
				"$schema": "https://pi.dev/schemas/keybindings.schema.json",
				"app.tools.expand": ["ctrl+y"],
				"tui.input.submit": "ctrl+m"
			}"#,
		)
		.unwrap();
		let ui = load_from(
			&dir.join("keybindings.json"),
			&dir.join("themes"),
			None,
			false,
		);
		assert!(ui.notices.is_empty(), "{:?}", ui.notices);
		let enter = pi_tui::parse_chord("enter").unwrap().to_key_event();
		let ctrl_y = pi_tui::parse_chord("ctrl+y").unwrap().to_key_event();
		let ctrl_o = pi_tui::parse_chord("ctrl+o").unwrap().to_key_event();
		let ctrl_m = pi_tui::parse_chord("ctrl+m").unwrap().to_key_event();
		assert!(ui.keybindings.matches("app.tools.expand", &ctrl_y));
		assert!(!ui.keybindings.matches("app.tools.expand", &ctrl_o));
		assert!(ui.keybindings.matches("tui.input.submit", &ctrl_m));
		assert!(!ui.keybindings.matches("tui.input.submit", &enter));
		// Untouched actions keep their defaults.
		assert!(ui.keybindings.matches(
			"app.thinking.toggle",
			&pi_tui::parse_chord("ctrl+t").unwrap().to_key_event()
		));
	}

	#[test]
	fn keybindings_bad_json_notices_and_keeps_defaults() {
		let (_, dir) = load_in("kb-bad-json", None, false);
		std::fs::write(dir.join("keybindings.json"), "{ not json").unwrap();
		let ui = load_from(
			&dir.join("keybindings.json"),
			&dir.join("themes"),
			None,
			false,
		);
		assert_eq!(ui.notices.len(), 1);
		assert!(
			ui.notices[0].contains("keybindings.json"),
			"{}",
			ui.notices[0]
		);
		assert!(ui.keybindings.matches(
			"app.tools.expand",
			&pi_tui::parse_chord("ctrl+o").unwrap().to_key_event()
		));
	}

	#[test]
	fn keybindings_non_object_notices() {
		let (_, dir) = load_in("kb-non-object", None, false);
		std::fs::write(dir.join("keybindings.json"), "[]").unwrap();
		let ui = load_from(
			&dir.join("keybindings.json"),
			&dir.join("themes"),
			None,
			false,
		);
		assert_eq!(ui.notices.len(), 1);
		assert!(ui.notices[0].contains("JSON object"));
	}

	#[test]
	fn keybindings_invalid_chord_notices_and_keeps_defaults() {
		let (_, dir) = load_in("kb-bad-chord", None, false);
		std::fs::write(
			dir.join("keybindings.json"),
			r#"{ "app.tools.expand": "not a chord+" }"#,
		)
		.unwrap();
		let ui = load_from(
			&dir.join("keybindings.json"),
			&dir.join("themes"),
			None,
			false,
		);
		assert_eq!(ui.notices.len(), 1);
		assert!(ui.notices[0].contains("invalid key chord"));
		assert!(ui.keybindings.matches(
			"app.tools.expand",
			&pi_tui::parse_chord("ctrl+o").unwrap().to_key_event()
		));
	}

	#[test]
	fn keybindings_schema_invalid_values_are_dropped() {
		let (_, dir) = load_in("kb-bad-shape", None, false);
		// 42 is neither a key id nor a list (upstream drops it silently).
		std::fs::write(
			dir.join("keybindings.json"),
			r#"{ "app.tools.expand": 42, "app.thinking.toggle": ["f2"] }"#,
		)
		.unwrap();
		let ui = load_from(
			&dir.join("keybindings.json"),
			&dir.join("themes"),
			None,
			false,
		);
		assert!(ui.notices.is_empty());
		assert!(ui.keybindings.matches(
			"app.tools.expand",
			&pi_tui::parse_chord("ctrl+o").unwrap().to_key_event()
		));
		assert!(ui.keybindings.matches(
			"app.thinking.toggle",
			&pi_tui::parse_chord("f2").unwrap().to_key_event()
		));
	}

	#[test]
	fn theme_defaults_follow_light_flag() {
		let (dark, _) = load_in("theme-flag-dark", None, false);
		let (light, _) = load_in("theme-flag-light", None, true);
		assert_eq!(dark.theme.name, Theme::dark().name);
		assert_eq!(light.theme.name, Theme::light().name);
	}

	#[test]
	fn theme_builtin_names_resolve_without_files() {
		let (dark, _) = load_in("theme-builtin-dark", Some("dark"), true);
		let (light, _) = load_in("theme-builtin-light", Some("light"), false);
		assert_eq!(dark.theme.name, Theme::dark().name);
		assert_eq!(light.theme.name, Theme::light().name);
		assert!(dark.notices.is_empty());
	}

	#[test]
	fn theme_custom_file_loads() {
		let (_, dir) = load_in("theme-custom", Some("smoke"), true);
		let themes = dir.join("themes");
		std::fs::create_dir_all(&themes).unwrap();
		std::fs::write(
			themes.join("smoke.json"),
			r##"{ "name": "smoke", "colors": { "text": "#ff0000" } }"##,
		)
		.unwrap();
		let ui = load_from(&dir.join("keybindings.json"), &themes, Some("smoke"), true);
		assert!(ui.notices.is_empty(), "{:?}", ui.notices);
		assert_eq!(ui.theme.name, "smoke");
		assert_eq!(ui.theme.assistant, Color::Rgb(255, 0, 0));
		// Uncovered tokens fall back to the dark baseline (from_json_str).
		assert_eq!(ui.theme.error, Theme::dark().error);
	}

	#[test]
	fn theme_missing_file_notices_and_falls_back() {
		let (light, dir) = load_in("theme-missing", Some("nope"), true);
		assert_eq!(light.theme.name, Theme::light().name);
		assert_eq!(light.notices.len(), 1);
		assert!(
			light.notices[0].contains(&dir.join("themes").join("nope.json").display().to_string()),
			"{}",
			light.notices[0]
		);
		let (dark, _) = load_in("theme-missing-dark", Some("nope"), false);
		assert_eq!(dark.theme.name, Theme::dark().name);
	}

	#[test]
	fn theme_broken_json_notices_and_falls_back() {
		let (_, dir) = load_in("theme-broken", Some("broken"), false);
		let themes = dir.join("themes");
		std::fs::create_dir_all(&themes).unwrap();
		std::fs::write(themes.join("broken.json"), "{ nope").unwrap();
		let ui = load_from(
			&dir.join("keybindings.json"),
			&themes,
			Some("broken"),
			false,
		);
		assert_eq!(ui.theme.name, Theme::dark().name);
		assert_eq!(ui.notices.len(), 1);
		assert!(ui.notices[0].contains("broken"), "{}", ui.notices[0]);
	}

	#[test]
	fn theme_reserved_slash_name_notices() {
		let (ui, _) = load_in("theme-slash", Some("light/dark"), false);
		assert_eq!(ui.theme.name, Theme::dark().name);
		assert_eq!(ui.notices.len(), 1);
		assert!(ui.notices[0].contains("light/dark"));
	}
}
