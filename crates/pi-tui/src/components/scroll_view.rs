//! Scroll view — counterpart of `pi/packages/tui/src/components/scroll-view.ts`.
//!
//! Holds rendered lines, tracks scroll offset, and follows the bottom as new
//! content arrives unless the user scrolled up.

use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Text as RatatuiText};
use ratatui::widgets::{
	Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget,
};

use super::Component;

pub struct ScrollView {
	lines: Vec<Line<'static>>,
	/// Vertical scroll offset in wrapped lines (approximated as raw lines).
	offset: usize,
	follow_bottom: bool,
}

impl ScrollView {
	pub fn new() -> Self {
		Self {
			lines: Vec::new(),
			offset: 0,
			follow_bottom: true,
		}
	}

	pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
		self.lines = lines;
		self.clamp();
	}

	pub fn push_line(&mut self, line: Line<'static>) {
		self.lines.push(line);
		self.clamp();
	}

	pub fn lines(&self) -> &[Line<'static>] {
		&self.lines
	}

	pub fn clear(&mut self) {
		self.lines.clear();
		self.offset = 0;
		self.follow_bottom = true;
	}

	pub fn scroll_up(&mut self, by: usize) {
		self.offset = self.offset.saturating_sub(by);
		self.follow_bottom = false;
	}

	pub fn scroll_down(&mut self, by: usize, viewport_height: usize) {
		let max = self.max_offset(viewport_height);
		self.offset = (self.offset + by).min(max);
		self.follow_bottom = self.offset >= max;
	}

	pub fn scroll_to_bottom(&mut self, viewport_height: usize) {
		self.offset = self.max_offset(viewport_height);
		self.follow_bottom = true;
	}

	fn max_offset(&self, viewport_height: usize) -> usize {
		self.lines.len().saturating_sub(viewport_height.max(1))
	}

	fn clamp(&mut self) {
		if self.follow_bottom {
			self.offset = usize::MAX; // sentinel; resolved at render with real height
		}
	}
}

impl Default for ScrollView {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for ScrollView {
	fn handle_event(&mut self, event: &Event) -> bool {
		match event {
			Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
				match key.code {
					KeyCode::PageUp => {
						self.scroll_up(20);
						true
					}
					KeyCode::PageDown => {
						self.scroll_down(20, usize::MAX); // clamped at render
						true
					}
					_ => false,
				}
			}
			Event::Mouse(mouse) => match mouse.kind {
				MouseEventKind::ScrollUp => {
					self.scroll_up(3);
					true
				}
				MouseEventKind::ScrollDown => {
					self.scroll_down(3, usize::MAX);
					true
				}
				MouseEventKind::Down(MouseButton::Left) => true,
				_ => false,
			},
			_ => false,
		}
	}

	fn render(&mut self, area: Rect, buf: &mut Buffer) {
		if area.height == 0 {
			return;
		}
		let viewport = area.height as usize;

		// Resolve sentinel / clamp with the real viewport height.
		let max = self.max_offset(viewport);
		if self.follow_bottom || self.offset == usize::MAX {
			self.offset = max;
		}
		self.offset = self.offset.min(max);

		let visible: Vec<Line<'static>> = self
			.lines
			.iter()
			.skip(self.offset)
			.take(viewport)
			.cloned()
			.collect();
		Paragraph::new(RatatuiText::from(visible)).render(area, buf);

		if max > 0 {
			let mut state = ScrollbarState::new(max).position(self.offset);
			Scrollbar::new(ScrollbarOrientation::VerticalRight).render(area, buf, &mut state);
		}
	}

	fn preferred_height(&self, _width: u16) -> u16 {
		self.lines.len() as u16
	}
}
