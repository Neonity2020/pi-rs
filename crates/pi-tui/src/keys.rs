//! Key handling — counterpart of `pi/packages/tui/src/keys.ts` (key
//! identifiers, chord parsing) and the crossterm-facing half of
//! `pi/packages/tui/src/keybindings.ts`.
//!
//! Upstream matches raw terminal byte sequences (legacy SS3/CSI, Kitty
//! keyboard protocol, xterm modifyOtherKeys); here crossterm does that
//! decoding and hands us [`KeyEvent`]s, so this layer starts from them:
//! [`parse_chord`] turns a chord string like `"ctrl+o"` into a
//! [`KeyChord`], and [`KeyChord::matches_event`] checks it against events.
//!
//! Naming follows upstream `KeyId`: modifiers are `ctrl`/`alt`/`shift`/
//! `super` (case-insensitive, repeatable modifiers are rejected), special
//! keys are `escape`/`esc`, `enter`/`return`, `tab`, `space`, `backspace`,
//! `delete`, `insert`, `clear`, `home`, `end`, `pageUp`, `pageDown`, the
//! arrows, and `f1`-`f12`; single letters, digits, and the 31 symbol keys
//! (`` ` `` `-` `=` `[` `]` `\` `;` `'` `,` `.` `/` `!` `@` `#` `$` `%` `^`
//! `&` `*` `(` `)` `_` `+` `|` `~` `{` `}` `:` `<` `>` `?`) are their own
//! names. Upstream quirks that are pure byte-level concerns (Kitty CSI-u
//! alternates, modifyOtherKeys, the Windows Terminal `0x08` ambiguity) are
//! intentionally not reproduced; `clear` parses (upstream has it) but
//! crossterm never reports such a key, so it matches nothing.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Human-readable key name, e.g. `ctrl+c`, `alt+enter`, `pgdn`.
pub fn key_event_name(event: &KeyEvent) -> String {
	let mut parts: Vec<String> = Vec::new();
	let modifiers = event.modifiers;
	if modifiers.contains(KeyModifiers::CONTROL) {
		parts.push("ctrl".into());
	}
	if modifiers.contains(KeyModifiers::ALT) {
		parts.push("alt".into());
	}
	if modifiers.contains(KeyModifiers::SHIFT) {
		parts.push("shift".into());
	}
	if modifiers.contains(KeyModifiers::SUPER) {
		parts.push("super".into());
	}
	let code = match event.code {
		KeyCode::Char(' ') => "space".to_string(),
		KeyCode::Char(c) => c.to_string(),
		KeyCode::Enter => "enter".into(),
		KeyCode::Esc => "escape".into(),
		KeyCode::Backspace => "backspace".into(),
		KeyCode::Delete => "delete".into(),
		KeyCode::Tab => "tab".into(),
		KeyCode::BackTab => "shift+tab".into(),
		KeyCode::Left => "left".into(),
		KeyCode::Right => "right".into(),
		KeyCode::Up => "up".into(),
		KeyCode::Down => "down".into(),
		KeyCode::Home => "home".into(),
		KeyCode::End => "end".into(),
		KeyCode::PageUp => "pgup".into(),
		KeyCode::PageDown => "pgdn".into(),
		KeyCode::Insert => "insert".into(),
		KeyCode::F(n) => format!("f{n}"),
		KeyCode::Null => "null".into(),
		KeyCode::CapsLock => "capslock".into(),
		KeyCode::Menu => "menu".into(),
		_ => "unknown".into(),
	};
	parts.push(code);
	parts.join("+")
}

/// The 31 symbol key names accepted upstream (`keys.ts` `SymbolKey`).
const SYMBOL_KEYS: &str = "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?";

/// Error returned when a chord string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseKeyError(pub String);

impl fmt::Display for ParseKeyError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "invalid key chord: {}", self.0)
	}
}

impl std::error::Error for ParseKeyError {}

/// The base (unmodified) key of a chord — upstream `BaseKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyName {
	/// A letter, digit, or symbol key (`keys.ts` letters/digits/`SymbolKey`).
	Char(char),
	Escape,
	Enter,
	Tab,
	Space,
	Backspace,
	Delete,
	Insert,
	/// Parseable for upstream parity, but crossterm never reports it.
	Clear,
	Home,
	End,
	PageUp,
	PageDown,
	Up,
	Down,
	Left,
	Right,
	F(u8),
}

impl KeyName {
	/// Canonical `KeyId` spelling (`pageUp` stays camelCase, like upstream).
	pub fn name(self) -> String {
		match self {
			KeyName::Char(c) => c.to_string(),
			KeyName::Escape => "escape".into(),
			KeyName::Enter => "enter".into(),
			KeyName::Tab => "tab".into(),
			KeyName::Space => "space".into(),
			KeyName::Backspace => "backspace".into(),
			KeyName::Delete => "delete".into(),
			KeyName::Insert => "insert".into(),
			KeyName::Clear => "clear".into(),
			KeyName::Home => "home".into(),
			KeyName::End => "end".into(),
			KeyName::PageUp => "pageUp".into(),
			KeyName::PageDown => "pageDown".into(),
			KeyName::Up => "up".into(),
			KeyName::Down => "down".into(),
			KeyName::Left => "left".into(),
			KeyName::Right => "right".into(),
			KeyName::F(n) => format!("f{n}"),
		}
	}

	fn parse(name: &str) -> Result<Self, ParseKeyError> {
		match name {
			"escape" | "esc" => return Ok(KeyName::Escape),
			"enter" | "return" => return Ok(KeyName::Enter),
			"tab" => return Ok(KeyName::Tab),
			"space" => return Ok(KeyName::Space),
			"backspace" => return Ok(KeyName::Backspace),
			"delete" => return Ok(KeyName::Delete),
			"insert" => return Ok(KeyName::Insert),
			"clear" => return Ok(KeyName::Clear),
			"home" => return Ok(KeyName::Home),
			"end" => return Ok(KeyName::End),
			"pageup" => return Ok(KeyName::PageUp),
			"pagedown" => return Ok(KeyName::PageDown),
			"up" => return Ok(KeyName::Up),
			"down" => return Ok(KeyName::Down),
			"left" => return Ok(KeyName::Left),
			"right" => return Ok(KeyName::Right),
			_ => {}
		}
		if let Some(digits) = name.strip_prefix('f')
			&& !digits.is_empty()
		{
			if let Ok(n) = digits.parse::<u8>()
				&& (1..=12).contains(&n)
			{
				return Ok(KeyName::F(n));
			}
			return Err(ParseKeyError(format!(
				"unsupported function key \"{name}\""
			)));
		}
		let mut chars = name.chars();
		if let (Some(c), None) = (chars.next(), chars.next()) {
			let is_letter = c.is_ascii_lowercase();
			let is_digit = c.is_ascii_digit();
			let is_symbol = SYMBOL_KEYS.contains(c);
			if is_letter || is_digit || is_symbol {
				return Ok(KeyName::Char(c));
			}
		}
		Err(ParseKeyError(format!("unknown key name \"{name}\"")))
	}
}

impl fmt::Display for KeyName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.name())
	}
}

/// A key plus held modifiers — the parseable form of an upstream `KeyId`
/// like `"ctrl+o"`, `"alt+backspace"`, `"ctrl+shift+pageUp"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyChord {
	pub ctrl: bool,
	pub shift: bool,
	pub alt: bool,
	/// `super` is a Rust keyword; this is upstream's `super+` modifier.
	pub super_key: bool,
	pub key: KeyName,
}

impl KeyChord {
	/// A chord with no modifiers.
	pub fn bare(key: KeyName) -> Self {
		KeyChord {
			ctrl: false,
			shift: false,
			alt: false,
			super_key: false,
			key,
		}
	}

	/// Parse a chord string (upstream `keys.ts` `parseKeyId`): modifiers as
	/// `+`-prefixed prefixes in any order, then a base key name. The whole
	/// string is case-insensitive; `"+"` is the plus-symbol key.
	pub fn parse(chord: &str) -> Result<Self, ParseKeyError> {
		let mut rest = chord.trim().to_ascii_lowercase();
		let (mut ctrl, mut shift, mut alt, mut super_key) = (false, false, false, false);
		loop {
			let (taken, modifier) = if let Some(tail) = rest.strip_prefix("ctrl+") {
				(tail, &mut ctrl)
			} else if let Some(tail) = rest.strip_prefix("shift+") {
				(tail, &mut shift)
			} else if let Some(tail) = rest.strip_prefix("alt+") {
				(tail, &mut alt)
			} else if let Some(tail) = rest.strip_prefix("super+") {
				(tail, &mut super_key)
			} else {
				break;
			};
			if *modifier {
				return Err(ParseKeyError(format!("duplicate modifier in \"{chord}\"")));
			}
			*modifier = true;
			rest = taken.to_string();
		}
		if rest.is_empty() || (rest.contains('+') && rest != "+") {
			return Err(ParseKeyError(format!("malformed key chord \"{chord}\"")));
		}
		Ok(KeyChord {
			ctrl,
			shift,
			alt,
			super_key,
			key: KeyName::parse(&rest)?,
		})
	}

	/// Whether a crossterm key event matches this chord. Release events never
	/// match; key repeats do. Modifiers must match exactly except that
	/// `shift` is also implied by an uppercase character (crossterm's Kitty
	/// path reports the shifted char and clears the SHIFT bit), and the
	/// legacy `BackTab` encoding of shift+tab is accepted for
	/// `shift+tab`.
	pub fn matches_event(&self, event: &KeyEvent) -> bool {
		if event.kind == KeyEventKind::Release {
			return false;
		}
		let key_matches = match (self.key, &event.code) {
			(KeyName::Char(c), KeyCode::Char(actual)) => actual.to_ascii_lowercase() == c,
			(KeyName::Space, KeyCode::Char(' ')) => true,
			(KeyName::Escape, KeyCode::Esc) => true,
			(KeyName::Enter, KeyCode::Enter) => true,
			(KeyName::Tab, KeyCode::Tab) => true,
			// `\x1b[Z` (back-tab) is how terminals send shift+tab.
			(KeyName::Tab, KeyCode::BackTab) => self.shift,
			(KeyName::Backspace, KeyCode::Backspace) => true,
			(KeyName::Delete, KeyCode::Delete) => true,
			(KeyName::Insert, KeyCode::Insert) => true,
			(KeyName::Clear, _) => false,
			(KeyName::Home, KeyCode::Home) => true,
			(KeyName::End, KeyCode::End) => true,
			(KeyName::PageUp, KeyCode::PageUp) => true,
			(KeyName::PageDown, KeyCode::PageDown) => true,
			(KeyName::Up, KeyCode::Up) => true,
			(KeyName::Down, KeyCode::Down) => true,
			(KeyName::Left, KeyCode::Left) => true,
			(KeyName::Right, KeyCode::Right) => true,
			(KeyName::F(expected), KeyCode::F(actual)) => expected == *actual,
			_ => false,
		};
		if !key_matches {
			return false;
		}
		if event.code == KeyCode::BackTab {
			// BackTab always means shift+tab; the SHIFT bit itself is noise.
			return self.key == KeyName::Tab
				&& self.shift
				&& self.ctrl == event.modifiers.contains(KeyModifiers::CONTROL)
				&& self.alt == event.modifiers.contains(KeyModifiers::ALT)
				&& self.super_key == event.modifiers.contains(KeyModifiers::SUPER);
		}
		if self.ctrl != event.modifiers.contains(KeyModifiers::CONTROL) {
			return false;
		}
		if self.alt != event.modifiers.contains(KeyModifiers::ALT) {
			return false;
		}
		if self.super_key != event.modifiers.contains(KeyModifiers::SUPER) {
			return false;
		}
		if self.shift != event_has_shift(event) {
			return false;
		}
		true
	}

	/// The crossterm event a chord decodes to (the inverse of
	/// [`KeyChord::matches_event`]); useful for tests and input synthesis.
	pub fn to_key_event(&self) -> KeyEvent {
		let mut modifiers = KeyModifiers::empty();
		if self.ctrl {
			modifiers |= KeyModifiers::CONTROL;
		}
		if self.alt {
			modifiers |= KeyModifiers::ALT;
		}
		if self.super_key {
			modifiers |= KeyModifiers::SUPER;
		}
		if self.shift {
			modifiers |= KeyModifiers::SHIFT;
		}
		let code = match self.key {
			KeyName::Char(c) => KeyCode::Char(if self.shift {
				c.to_ascii_uppercase()
			} else {
				c
			}),
			KeyName::Space => KeyCode::Char(' '),
			KeyName::Escape => KeyCode::Esc,
			KeyName::Enter => KeyCode::Enter,
			KeyName::Tab if self.shift => KeyCode::BackTab,
			KeyName::Tab => KeyCode::Tab,
			KeyName::Backspace => KeyCode::Backspace,
			KeyName::Delete => KeyCode::Delete,
			KeyName::Insert => KeyCode::Insert,
			KeyName::Clear => KeyCode::Null,
			KeyName::Home => KeyCode::Home,
			KeyName::End => KeyCode::End,
			KeyName::PageUp => KeyCode::PageUp,
			KeyName::PageDown => KeyCode::PageDown,
			KeyName::Up => KeyCode::Up,
			KeyName::Down => KeyCode::Down,
			KeyName::Left => KeyCode::Left,
			KeyName::Right => KeyCode::Right,
			KeyName::F(n) => KeyCode::F(n),
		};
		KeyEvent::new(code, modifiers)
	}
}

impl fmt::Display for KeyChord {
	/// Canonical spelling with modifiers in upstream
	/// `formatKeyNameWithModifiers` order: shift, ctrl, alt, super.
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let mut out = String::new();
		if self.shift {
			out.push_str("shift+");
		}
		if self.ctrl {
			out.push_str("ctrl+");
		}
		if self.alt {
			out.push_str("alt+");
		}
		if self.super_key {
			out.push_str("super+");
		}
		out.push_str(&self.key.name());
		f.write_str(&out)
	}
}

/// Parse a chord string like `"ctrl+o"` — see [`KeyChord::parse`].
pub fn parse_chord(chord: &str) -> Result<KeyChord, ParseKeyError> {
	KeyChord::parse(chord)
}

/// Whether the event carries a shift: the SHIFT modifier bit, or an
/// uppercase character (crossterm's Kitty path reports the shifted
/// character and clears the bit).
fn event_has_shift(event: &KeyEvent) -> bool {
	event.modifiers.contains(KeyModifiers::SHIFT)
		|| matches!(event.code, KeyCode::Char(c) if c.is_uppercase())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn chord(s: &str) -> KeyChord {
		parse_chord(s).unwrap()
	}

	fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
		KeyEvent::new(code, modifiers)
	}

	#[test]
	fn parses_chord_shapes() {
		assert_eq!(
			chord("ctrl+o"),
			KeyChord {
				ctrl: true,
				shift: false,
				alt: false,
				super_key: false,
				key: KeyName::Char('o')
			}
		);
		assert_eq!(
			chord("alt+backspace"),
			KeyChord {
				ctrl: false,
				shift: false,
				alt: true,
				super_key: false,
				key: KeyName::Backspace
			}
		);
		assert_eq!(chord("shift+enter").key, KeyName::Enter);
		assert_eq!(chord("up").key, KeyName::Up);
		assert_eq!(chord("pageUp").key, KeyName::PageUp);
		assert_eq!(chord("pagedown").key, KeyName::PageDown);
		assert_eq!(
			chord("ctrl+shift+up"),
			KeyChord {
				ctrl: true,
				shift: true,
				alt: false,
				super_key: false,
				key: KeyName::Up
			}
		);
		assert_eq!(chord("ctrl+-").key, KeyName::Char('-'));
		assert_eq!(chord("ctrl+]").key, KeyName::Char(']'));
		assert_eq!(chord("+").key, KeyName::Char('+'));
		// Case-insensitive, like upstream parseKeyId.
		assert_eq!(chord("CTRL+A"), chord("ctrl+a"));
		assert_eq!(chord("esc").key, KeyName::Escape);
		assert_eq!(chord("return").key, KeyName::Enter);
		assert_eq!(chord("f5").key, KeyName::F(5));
		assert_eq!(chord("f12").key, KeyName::F(12));
		assert!(chord("super+k").super_key);
		// Modifier order does not matter.
		assert_eq!(chord("shift+ctrl+p"), chord("ctrl+shift+p"));
	}

	#[test]
	fn rejects_malformed_chords() {
		assert!(parse_chord("").is_err());
		assert!(parse_chord("ctrl+").is_err());
		assert!(parse_chord("shift+").is_err());
		assert!(parse_chord("a+b").is_err());
		assert!(parse_chord("ctrl+ctrl+a").is_err());
		assert!(parse_chord("alt+shift+alt+x").is_err());
		assert!(parse_chord("f13").is_err());
		assert!(parse_chord("hyper+k").is_err());
		assert!(parse_chord("keypad+enter").is_err());
	}

	#[test]
	fn displays_canonical_chords() {
		assert_eq!(chord("ctrl+o").to_string(), "ctrl+o");
		assert_eq!(chord("shift+enter").to_string(), "shift+enter");
		assert_eq!(chord("alt+backspace").to_string(), "alt+backspace");
		assert_eq!(chord("ctrl+shift+pageUp").to_string(), "shift+ctrl+pageUp");
		assert_eq!(chord("ctrl+-").to_string(), "ctrl+-");
		assert_eq!(chord("up").to_string(), "up");
	}

	#[test]
	fn matches_unmodified_keys() {
		assert!(chord("up").matches_event(&event(KeyCode::Up, KeyModifiers::empty())));
		assert!(chord("enter").matches_event(&event(KeyCode::Enter, KeyModifiers::empty())));
		assert!(chord("escape").matches_event(&event(KeyCode::Esc, KeyModifiers::empty())));
		assert!(chord("a").matches_event(&event(KeyCode::Char('a'), KeyModifiers::empty())));
		assert!(chord("space").matches_event(&event(KeyCode::Char(' '), KeyModifiers::empty())));
		assert!(!chord("a").matches_event(&event(KeyCode::Char('b'), KeyModifiers::empty())));
		// Uppercase input implies shift, so plain "a" does not match.
		assert!(!chord("a").matches_event(&event(KeyCode::Char('A'), KeyModifiers::SHIFT)));
		// Modifiers must match exactly.
		assert!(!chord("a").matches_event(&event(KeyCode::Char('a'), KeyModifiers::CONTROL)));
	}

	#[test]
	fn matches_shifted_keys() {
		let shift = KeyModifiers::SHIFT;
		assert!(chord("shift+enter").matches_event(&event(KeyCode::Enter, shift)));
		assert!(chord("shift+a").matches_event(&event(KeyCode::Char('A'), shift)));
		// Kitty path: shifted char reported, SHIFT bit cleared.
		assert!(chord("shift+a").matches_event(&event(KeyCode::Char('A'), KeyModifiers::empty())));
		// Legacy shift+tab encoding is BackTab.
		assert!(chord("shift+tab").matches_event(&event(KeyCode::BackTab, shift)));
		assert!(chord("shift+tab").matches_event(&event(KeyCode::BackTab, KeyModifiers::empty())));
		assert!(!chord("tab").matches_event(&event(KeyCode::BackTab, shift)));
		assert!(chord("tab").matches_event(&event(KeyCode::Tab, KeyModifiers::empty())));
		assert!(!chord("tab").matches_event(&event(KeyCode::Tab, shift)));
	}

	#[test]
	fn matches_modified_keys() {
		let ctrl = KeyModifiers::CONTROL;
		let alt = KeyModifiers::ALT;
		let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
		assert!(chord("ctrl+o").matches_event(&event(KeyCode::Char('o'), ctrl)));
		assert!(chord("alt+backspace").matches_event(&event(KeyCode::Backspace, alt)));
		assert!(chord("ctrl+shift+up").matches_event(&event(KeyCode::Up, ctrl_shift)));
		assert!(chord("ctrl+-").matches_event(&event(KeyCode::Char('-'), ctrl)));
		assert!(chord("ctrl+]").matches_event(&event(KeyCode::Char(']'), ctrl)));
		assert!(chord("ctrl+space").matches_event(&event(KeyCode::Char(' '), ctrl)));
		assert!(chord("ctrl+f5").matches_event(&event(KeyCode::F(5), ctrl)));
		// Wrong or missing modifiers do not match.
		assert!(!chord("ctrl+o").matches_event(&event(KeyCode::Char('o'), KeyModifiers::empty())));
		assert!(!chord("ctrl+o").matches_event(&event(KeyCode::Char('o'), ctrl | alt)));
		assert!(!chord("ctrl+o").matches_event(&event(KeyCode::Char('x'), ctrl)));
		// Release events never match.
		let mut release = event(KeyCode::Char('o'), ctrl);
		release.kind = KeyEventKind::Release;
		assert!(!chord("ctrl+o").matches_event(&release));
	}

	#[test]
	fn to_key_event_roundtrips() {
		let samples = [
			"ctrl+o",
			"alt+backspace",
			"shift+enter",
			"up",
			"ctrl+shift+pageUp",
			"ctrl+-",
			"shift+tab",
			"enter",
			"escape",
			"space",
			"f9",
			"shift+f2",
		];
		for sample in samples {
			let chord = chord(sample);
			assert!(
				chord.matches_event(&chord.to_key_event()),
				"{sample} -> {:?}",
				chord.to_key_event()
			);
		}
	}
}
