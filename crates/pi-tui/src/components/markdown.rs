//! Markdown component — counterpart of
//! `pi/packages/tui/src/components/markdown.ts`.
//!
//! Framework stage: renders markdown as plain text with minimal structure
//! (headings/list bullets bolded). The full pulldown-cmark renderer with
//! syntax highlighting lands with the interactive-mode polish pass.

use crossterm::event::Event;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::Component;

pub struct Markdown {
	source: String,
	style: Style,
}

impl Markdown {
	pub fn new(source: impl Into<String>) -> Self {
		Self { source: source.into(), style: Style::default() }
	}

	pub fn set_source(&mut self, source: impl Into<String>) {
		self.source = source.into();
	}

	fn lines(&self) -> Vec<Line<'static>> {
		self.source
			.lines()
			.map(|line| {
				if line.starts_with("### ") {
					Line::from(Span::styled(line.to_string(), Style::new().add_modifier(Modifier::BOLD)))
				} else if line.starts_with("## ") || line.starts_with("# ") {
					Line::from(Span::styled(
						line.to_string(),
						Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
					))
				} else if line.starts_with("- ") || line.starts_with("* ") {
					Line::from(format!(" {line}"))
				} else if line.starts_with("```") {
					Line::styled(line.to_string(), Style::new().add_modifier(Modifier::DIM))
				} else {
					Line::styled(line.to_string(), self.style)
				}
			})
			.collect()
	}
}

impl Component for Markdown {
	fn handle_event(&mut self, _event: &Event) -> bool {
		false
	}

	fn render(&mut self, area: Rect, buf: &mut Buffer) {
		Paragraph::new(self.lines()).render(area, buf);
	}

	fn preferred_height(&self, _width: u16) -> u16 {
		self.source.lines().count() as u16
	}
}
