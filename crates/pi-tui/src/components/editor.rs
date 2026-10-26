//! Editor component — counterpart of `pi/packages/tui/src/editor-component.ts`.
//!
//! Emacs-style line editor with history (up/down), word navigation, kill
//! line, and submit. The TS editor's full keybinding manager, undo stack,
//! kill ring, and autocomplete plug in here later.

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
	/// User pressed enter: the submitted line is carried, the buffer cleared.
	Submit(String),
	/// Escape while empty (used by callers for interrupt/exit decisions).
	Escape,
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
	/// Single-line mode; multi-line composition (alt+enter etc.) lands later.
	multiline: bool,
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
			multiline: true,
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

	fn kill_to_start(&mut self) {
		let byte = char_to_byte_index(&self.buffer, self.cursor);
		self.buffer.truncate(byte);
		self.cursor = 0;
	}

	fn submit(&mut self) {
		let line = self.buffer.clone();
		if !line.trim().is_empty() {
			self.history.push(line.clone());
		}
		self.clear();
		self.pending.push(EditorEvent::Submit(line));
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

		match (ctrl, alt, code) {
			(false, false, KeyCode::Char(c)) => {
				self.insert_char(c);
				true
			}
			(false, _, KeyCode::Enter) => {
				self.submit();
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
			(true, _, KeyCode::Char('u')) => {
				self.kill_to_start();
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

	/// Screen column of the cursor within the editor area (inside borders).
	pub fn cursor_screen_x(&self, inner_width: usize) -> u16 {
		let before_cursor: String =
			self.buffer.chars().take(self.cursor).collect();
		let prompt_w = UnicodeWidthStr::width(self.prompt.as_str());
		let cursor_w = UnicodeWidthStr::width(before_cursor.as_str());
		let x = prompt_w + cursor_w;
		if inner_width > 0 && x >= inner_width {
			// Simple horizontal clamp; full horizontal scrolling lands later.
			(inner_width - 1) as u16
		} else {
			x as u16
		}
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
					if c == '\n' || c == '\r' {
						if self.multiline {
							self.insert_char(' ');
						}
					} else {
						self.insert_char(c);
					}
				}
				true
			}
			_ => false,
		}
	}

	fn render(&mut self, area: Rect, buf: &mut Buffer) {
		let text = Line::from(vec![
			Span::styled(self.prompt.clone(), Style::new().add_modifier(Modifier::BOLD)),
			Span::raw(self.buffer.clone()),
			Span::raw(" "),
		]);
		Paragraph::new(text).style(self.style).render(area, buf);
	}

	fn preferred_height(&self, _width: u16) -> u16 {
		1
	}
}

/// Cursor position helper for terminal-level cursor display.
pub fn editor_cursor_position(editor: &Editor, area: Rect) -> Position {
	Position { x: area.x + editor.cursor_screen_x(area.width as usize), y: area.y }
}
