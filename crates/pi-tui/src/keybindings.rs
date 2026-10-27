//! Keybindings manager — port of `pi/packages/tui/src/keybindings.ts`.
//!
//! An independent registry mapping action ids (`"tui.editor.cursorLeft"`)
//! to chords: a defaults table ([`TUI_KEYBINDINGS`], mirroring upstream's
//! `TUI_KEYBINDINGS` verbatim, including actions whose default is empty),
//! optional user overrides that replace the defaults per action, conflict
//! detection when one user-bound chord is claimed by several actions, and
//! event matching. It knows nothing about TUI state, so the interactive
//! mode can wire it up later and applications can supply their own
//! definition tables (upstream's declaration-merging equivalent).

use std::collections::HashMap;

use crossterm::event::KeyEvent;

use crate::keys::{KeyChord, ParseKeyError, parse_chord};

/// One action's default bindings and description (upstream
/// `KeybindingDefinition`).
#[derive(Debug, Clone, Copy)]
pub struct KeybindingDefinition {
	pub default_keys: &'static [&'static str],
	pub description: &'static str,
}

const fn def(
	default_keys: &'static [&'static str],
	description: &'static str,
) -> KeybindingDefinition {
	KeybindingDefinition {
		default_keys,
		description,
	}
}

/// A chord that user bindings claim for several actions at once (upstream
/// `KeybindingConflict`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
	pub key: KeyChord,
	pub actions: Vec<&'static str>,
}

/// Ordered definition table — the static counterpart of upstream's
/// `KeybindingDefinitions` record. Order is preserved for
/// [`KeybindingsManager::action_for_event`].
pub type KeybindingDefinitions = &'static [(&'static str, KeybindingDefinition)];

/// Upstream `TUI_KEYBINDINGS`, transcribed verbatim (defaults, descriptions,
/// and order).
pub const TUI_KEYBINDINGS: KeybindingDefinitions = &[
	("tui.editor.cursorUp", def(&["up"], "Move cursor up")),
	("tui.editor.cursorDown", def(&["down"], "Move cursor down")),
	(
		"tui.editor.historyPrevious",
		def(&[], "Select previous prompt history entry"),
	),
	(
		"tui.editor.historyNext",
		def(&[], "Select next prompt history entry"),
	),
	(
		"tui.editor.cursorLeft",
		def(&["left", "ctrl+b"], "Move cursor left"),
	),
	(
		"tui.editor.cursorRight",
		def(&["right", "ctrl+f"], "Move cursor right"),
	),
	(
		"tui.editor.cursorWordLeft",
		def(&["alt+left", "ctrl+left", "alt+b"], "Move cursor word left"),
	),
	(
		"tui.editor.cursorWordRight",
		def(
			&["alt+right", "ctrl+right", "alt+f"],
			"Move cursor word right",
		),
	),
	(
		"tui.editor.cursorLineStart",
		def(&["home", "ctrl+a"], "Move to line start"),
	),
	(
		"tui.editor.cursorLineEnd",
		def(&["end", "ctrl+e"], "Move to line end"),
	),
	(
		"tui.editor.jumpForward",
		def(&["ctrl+]"], "Jump forward to character"),
	),
	(
		"tui.editor.jumpBackward",
		def(&["ctrl+alt+]"], "Jump backward to character"),
	),
	(
		"tui.editor.pageUp",
		def(&["pageUp", "ctrl+pageUp"], "Page up"),
	),
	(
		"tui.editor.pageDown",
		def(&["pageDown", "ctrl+pageDown"], "Page down"),
	),
	(
		"tui.editor.deleteCharBackward",
		def(&["backspace"], "Delete character backward"),
	),
	(
		"tui.editor.deleteCharForward",
		def(&["delete", "ctrl+d"], "Delete character forward"),
	),
	(
		"tui.editor.deleteWordBackward",
		def(&["ctrl+w", "alt+backspace"], "Delete word backward"),
	),
	(
		"tui.editor.deleteWordForward",
		def(&["alt+d", "alt+delete"], "Delete word forward"),
	),
	(
		"tui.editor.deleteToLineStart",
		def(&["ctrl+u"], "Delete to line start"),
	),
	(
		"tui.editor.deleteToLineEnd",
		def(&["ctrl+k"], "Delete to line end"),
	),
	("tui.editor.yank", def(&["ctrl+y"], "Yank")),
	("tui.editor.yankPop", def(&["alt+y"], "Yank pop")),
	("tui.editor.undo", def(&["ctrl+-"], "Undo")),
	(
		"tui.input.newLine",
		def(&["shift+enter", "ctrl+j"], "Insert newline"),
	),
	("tui.input.submit", def(&["enter"], "Submit input")),
	("tui.input.tab", def(&["tab"], "Tab / autocomplete")),
	("tui.input.copy", def(&["ctrl+c"], "Copy selection")),
	("tui.select.up", def(&["up"], "Move selection up")),
	("tui.select.down", def(&["down"], "Move selection down")),
	("tui.select.pageUp", def(&["pageUp"], "Selection page up")),
	(
		"tui.select.pageDown",
		def(&["pageDown"], "Selection page down"),
	),
	("tui.select.confirm", def(&["enter"], "Confirm selection")),
	(
		"tui.select.cancel",
		def(&["escape", "ctrl+c"], "Cancel selection"),
	),
	// These intentionally shadow the unmodified editor bindings in fullscreen mode.
	(
		"tui.altScreen.pageUp",
		def(&["pageUp"], "Scroll viewport up one page"),
	),
	(
		"tui.altScreen.pageDown",
		def(&["pageDown"], "Scroll viewport down one page"),
	),
	(
		"tui.altScreen.halfPageUp",
		def(&[], "Scroll viewport up half a page"),
	),
	(
		"tui.altScreen.halfPageDown",
		def(&[], "Scroll viewport down half a page"),
	),
	(
		"tui.altScreen.lineUp",
		def(&[], "Scroll viewport up one line"),
	),
	(
		"tui.altScreen.lineDown",
		def(&[], "Scroll viewport down one line"),
	),
	(
		"tui.altScreen.previousPrompt",
		def(
			&["ctrl+shift+up", "ctrl+up"],
			"Jump to previous semantic prompt",
		),
	),
	(
		"tui.altScreen.nextPrompt",
		def(
			&["ctrl+shift+down", "ctrl+down"],
			"Jump to next semantic prompt",
		),
	),
	(
		"tui.altScreen.search",
		def(&["ctrl+shift+f"], "Search the primary scroll view"),
	),
	(
		"tui.altScreen.searchNext",
		def(&["enter", "ctrl+g"], "Select the next search match"),
	),
	(
		"tui.altScreen.searchPrevious",
		def(
			&["shift+enter", "ctrl+shift+g"],
			"Select the previous search match",
		),
	),
	(
		"tui.altScreen.searchClose",
		def(&["escape"], "Close transcript search"),
	),
	(
		"tui.altScreen.top",
		def(&["ctrl+home"], "Scroll viewport to top"),
	),
	(
		"tui.altScreen.bottom",
		def(&["ctrl+end"], "Scroll viewport to bottom"),
	),
];

/// Global keybinding registry (upstream `KeybindingsManager`).
pub struct KeybindingsManager {
	/// Owned because applications extend the static base table with their
	/// own definitions (upstream spreads `{ ...TUI_KEYBINDINGS, ...APP }`).
	definitions: Vec<(&'static str, KeybindingDefinition)>,
	user_bindings: HashMap<String, Vec<KeyChord>>,
	keys_by_action: HashMap<&'static str, Vec<KeyChord>>,
	conflicts: Vec<KeybindingConflict>,
}

impl Default for KeybindingsManager {
	fn default() -> Self {
		Self::new()
	}
}

impl KeybindingsManager {
	/// The upstream `TUI_KEYBINDINGS` table without user overrides.
	pub fn new() -> Self {
		Self::with_definitions(TUI_KEYBINDINGS)
	}

	/// A registry over a custom definition table (upstream passes app-level
	/// definitions merged with `TUI_KEYBINDINGS`).
	pub fn with_definitions(definitions: KeybindingDefinitions) -> Self {
		Self::with_merged_definitions(definitions, &[])
	}

	/// A registry over `base` extended with `extra` (upstream's
	/// `KEYBINDINGS = { ...TUI_KEYBINDINGS, ...APP_KEYBINDINGS }` merge).
	/// `base` entries keep precedence in [`Self::action_for_event`] order.
	pub fn with_merged_definitions(
		base: KeybindingDefinitions,
		extra: KeybindingDefinitions,
	) -> Self {
		let mut definitions: Vec<(&'static str, KeybindingDefinition)> = base.to_vec();
		definitions.extend(extra.iter().copied());
		let mut manager = KeybindingsManager {
			definitions,
			user_bindings: HashMap::new(),
			keys_by_action: HashMap::new(),
			conflicts: Vec::new(),
		};
		manager.rebuild();
		manager
	}

	/// Recompute per-action keys and conflicts (upstream `rebuild`): user
	/// bindings replace an action's defaults wholesale, unknown action ids
	/// are skipped, and a chord claimed by more than one user-bound action
	/// becomes a conflict.
	fn rebuild(&mut self) {
		self.keys_by_action.clear();
		self.conflicts.clear();

		let mut claims: HashMap<KeyChord, Vec<&'static str>> = HashMap::new();
		for (action, chords) in &self.user_bindings {
			let Some(&(known, _)) = self
				.definitions
				.iter()
				.find(|(id, _)| *id == action.as_str())
			else {
				continue;
			};
			for chord in normalize_keys(chords) {
				let claimants = claims.entry(chord).or_default();
				if !claimants.contains(&known) {
					claimants.push(known);
				}
			}
		}

		self.conflicts = claims
			.into_iter()
			.filter(|(_, actions)| actions.len() > 1)
			.map(|(key, mut actions)| {
				actions.sort_unstable();
				KeybindingConflict { key, actions }
			})
			.collect();
		self.conflicts.sort_unstable_by_key(|conflict| conflict.key);

		for (id, definition) in &self.definitions {
			let keys = match self.user_bindings.get(*id) {
				Some(chords) => normalize_keys(chords),
				None => normalize_keys(
					&definition
						.default_keys
						.iter()
						.filter_map(|key| parse_chord(key).ok())
						.collect::<Vec<_>>(),
				),
			};
			self.keys_by_action.insert(id, keys);
		}
	}

	/// Whether the event matches any of the action's current bindings.
	pub fn matches(&self, action: &str, event: &KeyEvent) -> bool {
		self.keys_by_action
			.get(action)
			.is_some_and(|keys| keys.iter().any(|chord| chord.matches_event(event)))
	}

	/// First action (in definition order) whose bindings match the event.
	pub fn action_for_event(&self, event: &KeyEvent) -> Option<&'static str> {
		self.definitions
			.iter()
			.find(|(id, _)| self.matches(id, event))
			.map(|(id, _)| *id)
	}

	/// The action's current bindings (defaults or user overrides).
	pub fn keys_for(&self, action: &str) -> Option<&[KeyChord]> {
		self.keys_by_action.get(action).map(Vec::as_slice)
	}

	/// The action's current bindings as canonical chord strings, for hints.
	pub fn formatted_keys(&self, action: &str) -> Option<Vec<String>> {
		self.keys_for(action)
			.map(|keys| keys.iter().map(ToString::to_string).collect())
	}

	/// The action's definition (defaults and description).
	pub fn definition(&self, action: &str) -> Option<&KeybindingDefinition> {
		self.definitions
			.iter()
			.find(|(id, _)| *id == action)
			.map(|(_, definition)| definition)
	}

	/// Conflicts between user bindings, sorted for deterministic output.
	pub fn conflicts(&self) -> &[KeybindingConflict] {
		&self.conflicts
	}

	/// The currently installed user bindings (canonical chord strings).
	pub fn user_bindings(&self) -> HashMap<&str, Vec<String>> {
		self.user_bindings
			.iter()
			.map(|(action, chords)| {
				(
					action.as_str(),
					chords.iter().map(ToString::to_string).collect(),
				)
			})
			.collect()
	}

	/// Replace the user bindings and rebuild. Invalid chords fail the whole
	/// call without mutating the manager; unknown action ids are ignored
	/// (as upstream does). An empty key list disables the action.
	pub fn set_user_bindings(
		&mut self,
		bindings: HashMap<String, Vec<String>>,
	) -> Result<(), ParseKeyError> {
		let mut parsed = HashMap::new();
		for (action, keys) in bindings {
			let mut chords = Vec::with_capacity(keys.len());
			for key in keys {
				chords.push(parse_chord(&key)?);
			}
			parsed.insert(action, chords);
		}
		self.user_bindings = parsed;
		self.rebuild();
		Ok(())
	}

	/// Every action with its resolved bindings, in definition order
	/// (upstream `getResolvedBindings`).
	pub fn resolved_bindings(&self) -> Vec<(&'static str, Vec<KeyChord>)> {
		self.definitions
			.iter()
			.map(|(id, _)| {
				(
					*id,
					self.keys_by_action.get(*id).cloned().unwrap_or_default(),
				)
			})
			.collect()
	}
}

/// Dedupe keys preserving first-seen order (upstream `normalizeKeys`).
fn normalize_keys(keys: &[KeyChord]) -> Vec<KeyChord> {
	let mut seen = Vec::new();
	for key in keys {
		if !seen.contains(key) {
			seen.push(*key);
		}
	}
	seen
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::keys::{KeyChord, KeyName};
	use crossterm::event::{KeyCode, KeyModifiers};

	fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
		KeyEvent::new(code, modifiers)
	}

	fn chords(default_keys: &[&str]) -> Vec<KeyChord> {
		default_keys
			.iter()
			.map(|key| parse_chord(key).unwrap())
			.collect()
	}

	#[test]
	fn default_table_matches_upstream() {
		assert_eq!(TUI_KEYBINDINGS.len(), 47);
		let by_id: HashMap<&str, &KeybindingDefinition> =
			TUI_KEYBINDINGS.iter().map(|(id, def)| (*id, def)).collect();
		assert_eq!(by_id["tui.editor.cursorUp"].default_keys, &["up"]);
		assert_eq!(
			by_id["tui.editor.cursorLeft"].default_keys,
			&["left", "ctrl+b"]
		);
		assert_eq!(
			by_id["tui.editor.cursorWordLeft"].default_keys,
			&["alt+left", "ctrl+left", "alt+b"]
		);
		assert_eq!(by_id["tui.editor.undo"].default_keys, &["ctrl+-"]);
		assert_eq!(
			by_id["tui.input.newLine"].default_keys,
			&["shift+enter", "ctrl+j"]
		);
		assert_eq!(
			by_id["tui.select.cancel"].default_keys,
			&["escape", "ctrl+c"]
		);
		assert_eq!(
			by_id["tui.altScreen.previousPrompt"].default_keys,
			&["ctrl+shift+up", "ctrl+up"]
		);
		assert_eq!(by_id["tui.altScreen.top"].default_keys, &["ctrl+home"]);
		assert_eq!(
			by_id["tui.altScreen.search"].default_keys,
			&["ctrl+shift+f"]
		);
		// Empty-default actions are preserved.
		assert_eq!(
			by_id["tui.editor.historyPrevious"].default_keys,
			&[] as &[&str]
		);
		assert_eq!(
			by_id["tui.altScreen.halfPageUp"].default_keys,
			&[] as &[&str]
		);
	}

	#[test]
	fn every_default_chord_parses() {
		for (id, definition) in TUI_KEYBINDINGS {
			for key in definition.default_keys {
				assert!(parse_chord(key).is_ok(), "{id}: {key}");
			}
		}
	}

	#[test]
	fn matches_with_multiple_keys_per_action() {
		let manager = KeybindingsManager::new();
		let ctrl_b = event(KeyCode::Char('b'), KeyModifiers::CONTROL);
		assert!(manager.matches(
			"tui.editor.cursorLeft",
			&event(KeyCode::Left, KeyModifiers::empty())
		));
		assert!(manager.matches("tui.editor.cursorLeft", &ctrl_b));
		assert!(!manager.matches(
			"tui.editor.cursorLeft",
			&event(KeyCode::Right, KeyModifiers::empty())
		));

		// Same action bound to ctrl+w and alt+backspace.
		assert!(manager.matches(
			"tui.editor.deleteWordBackward",
			&event(KeyCode::Char('w'), KeyModifiers::CONTROL)
		));
		assert!(manager.matches(
			"tui.editor.deleteWordBackward",
			&event(KeyCode::Backspace, KeyModifiers::ALT)
		));
		assert_eq!(
			manager.keys_for("tui.editor.deleteWordBackward").unwrap(),
			chords(&["ctrl+w", "alt+backspace"]).as_slice()
		);
	}

	#[test]
	fn action_for_event_returns_first_definition_order_match() {
		let manager = KeybindingsManager::new();
		// "up" is bound to tui.editor.cursorUp first, then tui.select.up.
		assert_eq!(
			manager.action_for_event(&event(KeyCode::Up, KeyModifiers::empty())),
			Some("tui.editor.cursorUp")
		);
		assert!(manager.matches("tui.select.up", &event(KeyCode::Up, KeyModifiers::empty())));
		// Keys bound to no action resolve to None.
		assert_eq!(
			manager.action_for_event(&event(KeyCode::F(9), KeyModifiers::empty())),
			None
		);
		assert_eq!(
			manager.action_for_event(&event(KeyCode::Char('z'), KeyModifiers::empty())),
			None
		);
	}

	#[test]
	fn user_bindings_replace_defaults() {
		let mut manager = KeybindingsManager::new();
		manager
			.set_user_bindings(HashMap::from([(
				"tui.input.submit".to_string(),
				vec!["ctrl+m".to_string()],
			)]))
			.unwrap();

		assert!(!manager.matches(
			"tui.input.submit",
			&event(KeyCode::Enter, KeyModifiers::empty())
		));
		assert!(manager.matches(
			"tui.input.submit",
			&event(KeyCode::Char('m'), KeyModifiers::CONTROL)
		));
		assert_eq!(
			manager.keys_for("tui.input.submit").unwrap(),
			chords(&["ctrl+m"]).as_slice()
		);
		// Other actions keep their defaults.
		assert!(manager.matches(
			"tui.select.confirm",
			&event(KeyCode::Enter, KeyModifiers::empty())
		));
	}

	#[test]
	fn user_bindings_can_disable_actions() {
		let mut manager = KeybindingsManager::new();
		manager
			.set_user_bindings(HashMap::from([("tui.select.cancel".to_string(), vec![])]))
			.unwrap();
		assert_eq!(manager.keys_for("tui.select.cancel").unwrap(), &[]);
		assert!(!manager.matches(
			"tui.select.cancel",
			&event(KeyCode::Esc, KeyModifiers::empty())
		));
	}

	#[test]
	fn user_bindings_detect_conflicts_and_skip_unknown_actions() {
		let mut manager = KeybindingsManager::new();
		manager
			.set_user_bindings(HashMap::from([
				("tui.input.submit".to_string(), vec!["ctrl+m".to_string()]),
				("tui.input.tab".to_string(), vec!["ctrl+m".to_string()]),
				("app.interrupt".to_string(), vec!["escape".to_string()]),
			]))
			.unwrap();

		let conflicts = manager.conflicts();
		assert_eq!(conflicts.len(), 1);
		assert_eq!(conflicts[0].key, parse_chord("ctrl+m").unwrap());
		assert_eq!(conflicts[0].actions, &["tui.input.submit", "tui.input.tab"]);

		// Unknown action ids are ignored, no phantom entries.
		assert!(manager.keys_for("app.interrupt").is_none());
		assert_eq!(manager.resolved_bindings().len(), TUI_KEYBINDINGS.len());
	}

	#[test]
	fn invalid_user_bindings_fail_without_mutating() {
		let mut manager = KeybindingsManager::new();
		let before = manager.resolved_bindings();
		let error = manager
			.set_user_bindings(HashMap::from([(
				"tui.input.submit".to_string(),
				vec!["not a chord+".to_string()],
			)]))
			.unwrap_err();
		assert!(error.to_string().contains("not a chord+"));
		assert_eq!(manager.resolved_bindings(), before);
	}

	#[test]
	fn formatted_keys_and_definitions_are_exposed() {
		let manager = KeybindingsManager::new();
		assert_eq!(
			manager.formatted_keys("tui.editor.cursorWordLeft").unwrap(),
			vec!["alt+left", "ctrl+left", "alt+b"]
		);
		let definition = manager.definition("tui.editor.undo").unwrap();
		assert_eq!(definition.description, "Undo");
		assert!(manager.definition("app.interrupt").is_none());
	}

	#[test]
	fn custom_definition_tables_work() {
		const APP_TABLE: KeybindingDefinitions = &[(
			"app.interrupt",
			KeybindingDefinition {
				default_keys: &["escape"],
				description: "Cancel or abort",
			},
		)];
		let manager = KeybindingsManager::with_definitions(APP_TABLE);
		assert_eq!(manager.resolved_bindings().len(), 1);
		assert!(manager.matches("app.interrupt", &event(KeyCode::Esc, KeyModifiers::empty())));
		assert_eq!(
			manager.action_for_event(&event(KeyCode::Esc, KeyModifiers::empty())),
			Some("app.interrupt")
		);
		// Bare KeyChord construction also matches.
		let chord = KeyChord::bare(KeyName::Escape);
		assert!(chord.matches_event(&event(KeyCode::Esc, KeyModifiers::empty())));
	}

	#[test]
	fn merged_definitions_keep_base_order_and_resolve_extras() {
		const EXTRA: KeybindingDefinitions = &[(
			"app.interrupt",
			KeybindingDefinition {
				default_keys: &["escape"],
				description: "Cancel or abort",
			},
		)];
		let manager = KeybindingsManager::with_merged_definitions(TUI_KEYBINDINGS, EXTRA);
		assert_eq!(manager.resolved_bindings().len(), TUI_KEYBINDINGS.len() + 1);
		// Base entries keep precedence: escape resolves to the earlier
		// tui.select.cancel, not the appended app.interrupt.
		assert_eq!(
			manager.action_for_event(&event(KeyCode::Esc, KeyModifiers::empty())),
			Some("tui.select.cancel")
		);
		assert!(manager.matches("app.interrupt", &event(KeyCode::Esc, KeyModifiers::empty())));
		// User overrides still apply across both halves of the table.
		let mut manager = manager;
		manager
			.set_user_bindings(HashMap::from([(
				"app.interrupt".to_string(),
				vec!["ctrl+x".to_string()],
			)]))
			.unwrap();
		assert!(manager.matches(
			"app.interrupt",
			&event(KeyCode::Char('x'), KeyModifiers::CONTROL)
		));
		assert!(!manager.matches("app.interrupt", &event(KeyCode::Esc, KeyModifiers::empty())));
	}
}
