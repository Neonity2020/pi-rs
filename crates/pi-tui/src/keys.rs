//! Key handling helpers — counterpart of `pi/packages/tui/src/keys.ts` and
//! `keybindings.ts`. Full keybinding-manager config lands with the
//! interactive-mode polish pass.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
