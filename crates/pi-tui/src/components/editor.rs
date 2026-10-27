//! Editor component — counterpart of `pi/packages/tui/src/components/editor.ts`.
//!
//! Multi-line emacs-style editor: history (up/down), word navigation and
//! killing (readline semantics matching pi's word-navigation.ts), `\`+enter
//! line continuation (pi's soft-submit), kill ring basics. The TS editor's
//! full keybinding manager, undo stack and autocomplete plug in later.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::Component;

/// Events the editor surfaces to its owner (the interactive mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
	/// User pressed enter: the submitted text is carried, the buffer cleared.
	Submit(String),
	/// Escape while empty (used by callers for interrupt/exit decisions).
	Escape,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CharClass {
	Whitespace,
	Word,
	Punct,
}

fn classify(c: char) -> CharClass {
	if c.is_whitespace() {
		CharClass::Whitespace
	} else if c.is_alphanumeric() || !c.is_ascii() {
		CharClass::Word
	} else {
		CharClass::Punct
	}
}

/// Cursor position (in chars) after moving one word backward: skip trailing
/// whitespace, then one word or punctuation run (pi word-navigation.ts).
pub fn find_word_backward(text: &str, cursor: usize) -> usize {
	let chars: Vec<char> = text.chars().collect();
	let mut i = cursor.min(chars.len());
	while i > 0 && classify(chars[i - 1]) == CharClass::Whitespace {
		i -= 1;
	}
	if i == 0 {
		return i;
	}
	let class = classify(chars[i - 1]);
	while i > 0 && classify(chars[i - 1]) == class {
		i -= 1;
	}
	i
}

/// Cursor position (in chars) after moving one word forward.
pub fn find_word_forward(text: &str, cursor: usize) -> usize {
	let chars: Vec<char> = text.chars().collect();
	let mut i = cursor.min(chars.len());
	while i < chars.len() && classify(chars[i]) == CharClass::Whitespace {
		i += 1;
	}
	if i >= chars.len() {
		return chars.len();
	}
	let class = classify(chars[i]);
	while i < chars.len() && classify(chars[i]) == class {
		i += 1;
	}
	i
}

pub struct Editor {
	buffer: String,
	/// Cursor as char index into `buffer`.
	cursor: usize,
	history: Vec<String>,
	history_index: Option<usize>,
	prompt: String,
	style: Style,
	pending: Vec<EditorEvent>,
}

impl Editor {
	pub fn new(prompt: impl Into<String>) -> Self {
		Self {
			buffer: String::new(),
			cursor: 0,
			history: Vec::new(),
			history_index: None,
			prompt: prompt.into(),
			style: Style::default(),
			pending: Vec::new(),
		}
	}

	pub fn with_style(mut self, style: Style) -> Self {
		self.style = style;
		self
	}

	pub fn text(&self) -> &str {
		&self.buffer
	}

	pub fn set_text(&mut self, text: impl Into<String>) {
		self.buffer = text.into();
		self.cursor = self.buffer.chars().count();
	}

	pub fn clear(&mut self) {
		self.buffer.clear();
		self.cursor = 0;
		self.history_index = None;
	}

	pub fn is_empty(&self) -> bool {
		self.buffer.is_empty()
	}

	/// Take queued editor events (submit/escape).
	pub fn take_events(&mut self) -> Vec<EditorEvent> {
		std::mem::take(&mut self.pending)
	}

	fn insert_char(&mut self, c: char) {
		let byte = char_to_byte_index(&self.buffer, self.cursor);
		self.buffer.insert(byte, c);
		self.cursor += 1;
	}

	fn insert_str(&mut self, text: &str) {
		let byte = char_to_byte_index(&self.buffer, self.cursor);
		self.buffer.insert_str(byte, text);
		self.cursor += text.chars().count();
	}

	fn backspace(&mut self) {
		if self.cursor == 0 {
			return;
		}
		self.cursor -= 1;
		let byte = char_to_byte_index(&self.buffer, self.cursor);
		self.buffer.remove(byte);
	}

	fn delete_forward(&mut self) {
		if self.cursor >= self.buffer.chars().count() {
			return;
		}
		let byte = char_to_byte_index(&self.buffer, self.cursor);
		self.buffer.remove(byte);
	}

	fn kill_range(&mut self, from: usize, to: usize) {
		let (lo, hi) = (from.min(to), from.max(to));
		let start = char_to_byte_index(&self.buffer, lo);
		let end = char_to_byte_index(&self.buffer, hi);
		self.buffer.replace_range(start..end, "");
		self.cursor = lo;
	}

	fn move_left(&mut self) {
		self.cursor = self.cursor.saturating_sub(1);
	}

	fn move_right(&mut self) {
		if self.cursor < self.buffer.chars().count() {
			self.cursor += 1;
		}
	}

	fn move_home(&mut self) {
		self.cursor = 0;
	}

	fn move_end(&mut self) {
		self.cursor = self.buffer.chars().count();
	}

	/// Enter handling with pi's continuation rule: a `\` right before the
	/// cursor is removed and a newline inserted instead of submitting.
	fn submit_or_continue(&mut self) {
		if self.buffer[..char_to_byte_index(&self.buffer, self.cursor)].ends_with('\\') {
			// Drop the backslash and open a continuation line.
			self.cursor -= 1;
			let byte = char_to_byte_index(&self.buffer, self.cursor);
			self.buffer.remove(byte);
			self.insert_char('\n');
			return;
		}
		let line = self.buffer.clone();
		if !line.trim().is_empty() {
			self.history.push(line.clone());
		}
		self.clear();
		self.pending.push(EditorEvent::Submit(line));
	}

	fn insert_newline(&mut self) {
		self.insert_char('\n');
	}

	fn history_prev(&mut self) {
		if self.history.is_empty() {
			return;
		}
		let index = match self.history_index {
			None => self.history.len() - 1,
			Some(0) => 0,
			Some(i) => i - 1,
		};
		self.history_index = Some(index);
		self.set_text(self.history[index].clone());
	}

	fn history_next(&mut self) {
		let index = match self.history_index {
			None => return,
			Some(i) if i + 1 >= self.history.len() => {
				self.history_index = None;
				self.clear();
				return;
			}
			Some(i) => i + 1,
		};
		self.history_index = Some(index);
		self.set_text(self.history[index].clone());
	}

	fn handle_key(&mut self, event: KeyEvent) -> bool {
		let code = event.code;
		let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
		let alt = event.modifiers.contains(KeyModifiers::ALT);
		let shift = event.modifiers.contains(KeyModifiers::SHIFT);

		match (ctrl, alt, code) {
			(false, false, KeyCode::Char(c)) => {
				self.insert_char(c);
				true
			}
			// alt+enter / shift+enter: hard newline (terminals that support it).
			(false, true, KeyCode::Enter) | (false, false, KeyCode::Enter) if shift => {
				self.insert_newline();
				true
			}
			(false, _, KeyCode::Enter) => {
				self.submit_or_continue();
				true
			}
			(_, true, KeyCode::Enter) => {
				self.insert_newline();
				true
			}
			(_, _, KeyCode::Backspace) if alt => {
				let target = find_word_backward(&self.buffer, self.cursor);
				self.kill_range(target, self.cursor);
				true
			}
			(true, _, KeyCode::Char('w')) => {
				let target = find_word_backward(&self.buffer, self.cursor);
				self.kill_range(target, self.cursor);
				true
			}
			(_, _, KeyCode::Backspace) => {
				self.backspace();
				true
			}
			(false, _, KeyCode::Delete) => {
				self.delete_forward();
				true
			}
			(true, _, KeyCode::Char('k')) => {
				self.kill_range(self.cursor, self.buffer.chars().count());
				true
			}
			(true, _, KeyCode::Char('u')) => {
				self.kill_range(0, self.cursor);
				true
			}
			(true, _, KeyCode::Char('d')) => {
				self.delete_forward();
				true
			}
			(_, true, KeyCode::Char('d')) => {
				let target = find_word_forward(&self.buffer, self.cursor);
				self.kill_range(self.cursor, target);
				true
			}
			(_, true, KeyCode::Char('b')) => {
				self.cursor = find_word_backward(&self.buffer, self.cursor);
				true
			}
			(_, true, KeyCode::Char('f')) => {
				self.cursor = find_word_forward(&self.buffer, self.cursor);
				true
			}
			(false, _, KeyCode::Left) if ctrl => {
				self.cursor = find_word_backward(&self.buffer, self.cursor);
				true
			}
			(false, _, KeyCode::Right) if ctrl => {
				self.cursor = find_word_forward(&self.buffer, self.cursor);
				true
			}
			(false, _, KeyCode::Left) => {
				self.move_left();
				true
			}
			(false, _, KeyCode::Right) => {
				self.move_right();
				true
			}
			(false, _, KeyCode::Home) => {
				self.move_home();
				true
			}
			(false, _, KeyCode::End) => {
				self.move_end();
				true
			}
			(false, _, KeyCode::Up) => {
				self.history_prev();
				true
			}
			(false, _, KeyCode::Down) => {
				self.history_next();
				true
			}
			(true, _, KeyCode::Char('a')) => {
				self.move_home();
				true
			}
			(true, _, KeyCode::Char('e')) => {
				self.move_end();
				true
			}
			(false, _, KeyCode::Esc) => {
				self.pending.push(EditorEvent::Escape);
				true
			}
			_ => false,
		}
	}

	/// Screen column of the cursor for a single-row editor area (inside borders).
	/// Multi-line aware callers should prefer [`Self::cursor_screen_pos`].
	pub fn cursor_screen_x(&self, inner_width: usize) -> u16 {
		let (x, _) = self.cursor_xy(usize::MAX);
		self.clamp_x(x, inner_width)
	}

	/// (line index, column in chars) of the cursor.
	pub fn cursor_line_col(&self) -> (usize, usize) {
		let before: String = self.buffer.chars().take(self.cursor).collect();
		let line = before.chars().filter(|&c| c == '\n').count();
		let col = before.chars().rev().take_while(|&c| c != '\n').count();
		(line, col)
	}

	/// Rendered text lines (buffer split on newlines; prompt on the first).
	pub fn render_lines(&self) -> Vec<Line<'static>> {
		let prompt_style = Style::new().add_modifier(Modifier::BOLD).patch(self.style);
		let mut lines = Vec::new();
		if self.buffer.is_empty() {
			lines.push(Line::from(vec![
				Span::styled(self.prompt.clone(), prompt_style),
				Span::raw(" "),
			]));
			return lines;
		}
		for (i, line) in self.buffer.split('\n').enumerate() {
			if i == 0 {
				lines.push(Line::from(vec![
					Span::styled(self.prompt.clone(), prompt_style),
					Span::raw(line.to_string()),
				]));
			} else {
				lines.push(Line::from(vec![
					Span::raw("  ".to_string()),
					Span::raw(line.to_string()),
				]));
			}
		}
		lines
	}

	fn clamp_x(&self, x: usize, inner_width: usize) -> u16 {
		if inner_width > 0 && x >= inner_width {
			(inner_width - 1) as u16
		} else {
			x as u16
		}
	}

	/// Screen position (x, y relative to the editor's inner area) of the cursor,
	/// accounting for prompt width, multi-line rows, vertical overflow skipping
	/// and horizontal clamping.
	pub fn cursor_screen_pos(&self, area: Rect) -> (u16, u16) {
		let height = area.height as usize;
		let (x, y) = self.cursor_xy(height);
		(self.clamp_x(x, area.width as usize), (y as u16).min(area.height.saturating_sub(1)))
	}

	fn cursor_xy(&self, max_height: usize) -> (usize, usize) {
		let (row, col) = self.cursor_line_col();
		let text: Vec<&str> = self.buffer.split('\n').collect();
		let col_w = text
			.get(row)
			.map(|l| {
				let byte = l.char_indices().nth(col).map(|(b, _)| b).unwrap_or(l.len());
				UnicodeWidthStr::width(&l[..byte])
			})
			.unwrap_or(0);
		let x = if row == 0 {
			UnicodeWidthStr::width(self.prompt.as_str()) + col_w
		} else {
			2 + col_w
		};
		// Vertical overflow: the render skips leading lines to keep the cursor
		// row visible (bottom-anchored).
		let total = text.len();
		let skip = total.saturating_sub(max_height);
		let visible_row = row.saturating_sub(skip);
		(x, visible_row)
	}
}

fn char_to_byte_index(s: &str, char_index: usize) -> usize {
	s.char_indices().nth(char_index).map(|(b, _)| b).unwrap_or(s.len())
}

impl Component for Editor {
	fn handle_event(&mut self, event: &Event) -> bool {
		match event {
			Event::Key(key) => {
				if key.kind == crossterm::event::KeyEventKind::Release {
					return false;
				}
				self.handle_key(*key)
			}
			Event::Paste(data) => {
				for c in data.chars() {
					match c {
						'\n' | '\r' => self.insert_newline(),
						'\t' => self.insert_str("  "),
						c if (c as u32) < 32 => {}
						c => self.insert_char(c),
					}
				}
				true
			}
			_ => false,
		}
	}

	fn render(&mut self, area: Rect, buf: &mut Buffer) {
		let lines = self.render_lines();
		let height = (area.height as usize).min(lines.len());
		let skip = lines.len().saturating_sub(height);
		let visible: Vec<Line<'static>> = lines.into_iter().skip(skip).collect();
		Paragraph::new(ratatui::text::Text::from(visible)).style(self.style).render(area, buf);
	}

	fn preferred_height(&self, _width: u16) -> u16 {
		self.buffer.split('\n').count() as u16
	}
}

/// Cursor position helper for terminal-level cursor display.
pub fn editor_cursor_position(editor: &Editor, area: Rect) -> Position {
	let (x, y) = editor.cursor_screen_pos(area);
	Position { x, y: area.y + y }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn word_backward_skips_whitespace_then_run() {
		assert_eq!(find_word_backward("hello world", 11), 6);
		assert_eq!(find_word_backward("hello world", 6), 0);
		assert_eq!(find_word_backward("foo.bar baz", 7), 4); // before "bar", after "."
		assert_eq!(find_word_backward("  x", 3), 2);
	}

	#[test]
	fn word_forward_skips_whitespace_then_run() {
		assert_eq!(find_word_forward("hello world", 0), 5);
		assert_eq!(find_word_forward("  hello", 0), 7);
		assert_eq!(find_word_forward("foo.bar", 0), 3); // stops at "."
	}

	#[test]
	fn continuation_enter() {
		let mut editor = Editor::new("❯ ");
		editor.set_text("a\\");
		editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
		assert_eq!(editor.text(), "a\n");
		assert!(editor.take_events().is_empty());
		editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
		assert_eq!(editor.text(), "");
		assert_eq!(editor.take_events(), vec![EditorEvent::Submit("a\n".into())]);
	}

	#[test]
	fn ctrl_k_kills_to_end() {
		let mut editor = Editor::new("❯ ");
		editor.set_text("hello world");
		editor.cursor = 5;
		editor.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
		assert_eq!(editor.text(), "hello");
	}

	#[test]
	fn alt_backspace_kills_word() {
		let mut editor = Editor::new("❯ ");
		editor.set_text("hello world");
		editor.handle_key(KeyEvent::new(
			KeyCode::Backspace,
			KeyModifiers::ALT,
		));
		assert_eq!(editor.text(), "hello ");
	}

	#[test]
	fn multiline_render_and_cursor() {
		let mut editor = Editor::new("❯ ");
		editor.set_text("l1\nl22");
		editor.cursor = 5; // end of buffer → row 1, col 2
		let (row, col) = editor.cursor_line_col();
		assert_eq!((row, col), (1, 2));
		let lines = editor.render_lines();
		assert_eq!(lines.len(), 2);
	}
}
