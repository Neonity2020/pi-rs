//! Text component — counterpart of `pi/packages/tui/src/components/text.ts`.

use crossterm::event::Event;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text as RatatuiText};
use ratatui::widgets::{Paragraph, Wrap, Widget};

use super::Component;

pub struct Text {
	lines: Vec<Line<'static>>,
	wrap: bool,
	style: Style,
}

impl Text {
	pub fn new(content: impl Into<String>) -> Self {
		let lines: Vec<Line<'static>> =
			content.into().lines().map(|l| Line::raw(l.to_string())).collect();
		Self { lines, wrap: true, style: Style::default() }
	}

	pub fn styled(content: impl Into<String>, style: Style) -> Self {
		Self { lines: vec![Line::styled(content.into(), style)], wrap: true, style }
	}

	pub fn from_lines(lines: Vec<Line<'static>>) -> Self {
		Self { lines, wrap: false, style: Style::default() }
	}

	pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
		self.lines = lines;
	}

	pub fn lines(&self) -> &[Line<'static>] {
		&self.lines
	}
}

impl Component for Text {
	fn handle_event(&mut self, _event: &Event) -> bool {
		false
	}

	fn render(&mut self, area: Rect, buf: &mut Buffer) {
		let paragraph = Paragraph::new(RatatuiText::from(self.lines.clone()))
			.style(self.style)
			.wrap(Wrap { trim: self.wrap });
		paragraph.render(area, buf);
	}

	fn preferred_height(&self, width: u16) -> u16 {
		if width == 0 {
			return self.lines.len() as u16;
		}
		// Rough wrap estimate: expanded line count at the given width.
		let mut total: u16 = 0;
		for line in &self.lines {
			let width_len = line.width() as u16;
			total += (width_len / width).max(1);
		}
		total
	}
}

/// Convenience span builder used across components.
pub fn span(content: impl Into<String>, style: Style) -> Span<'static> {
	Span::styled(content.into(), style)
}
