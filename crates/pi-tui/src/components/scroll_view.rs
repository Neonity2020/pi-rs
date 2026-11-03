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
use crate::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines, wheel_now_ms};

/// PageUp/PageDown scroll all but this many lines, so the page turn keeps a
/// few lines of context visible (`tui-alt-screen.ts` `PAGE_SCROLL_OVERLAP`).
const PAGE_SCROLL_OVERLAP: usize = 4;

pub struct ScrollView {
	lines: Vec<Line<'static>>,
	/// Vertical scroll offset in wrapped lines (approximated as raw lines).
	offset: usize,
	follow_bottom: bool,
	/// Viewport height from the last render — the clamp anchor for wheel and
	/// page scrolling between frames (upstream reads `currentViewportHeight`).
	last_viewport: usize,
	/// Wheel event → line count (upstream `WheelScrollAccelerator`).
	wheel: WheelScrollAccelerator,
}

impl ScrollView {
	pub fn new() -> Self {
		Self {
			lines: Vec::new(),
			offset: 0,
			follow_bottom: true,
			last_viewport: 1,
			wheel: WheelScrollAccelerator::new(WheelScrollLines::Auto),
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

	/// Signed scroll in lines, clamped against the last rendered viewport and
	/// re-arming bottom-follow when the scroll lands at the end (upstream
	/// `scroll-view.ts` `scrollBy`). Resolves the follow sentinel first so
	/// arithmetic never runs on it.
	pub fn scroll_by(&mut self, lines: isize) {
		if lines == 0 {
			return;
		}
		let max = self.max_offset(self.last_viewport);
		if self.follow_bottom || self.offset == usize::MAX || self.offset > max {
			self.offset = max;
		}
		let target = (self.offset as isize + lines).clamp(0, max as isize) as usize;
		self.offset = target;
		self.follow_bottom = self.offset >= max;
	}

	pub fn scroll_up(&mut self, by: usize) {
		self.scroll_by(-(by as isize));
	}

	pub fn scroll_down(&mut self, by: usize) {
		self.scroll_by(by as isize);
	}

	pub fn scroll_to_bottom(&mut self) {
		self.follow_bottom = true;
		self.offset = usize::MAX; // sentinel; resolved at render
	}

	fn max_offset(&self, viewport_height: usize) -> usize {
		self.lines.len().saturating_sub(viewport_height.max(1))
	}

	fn page_step(&self) -> usize {
		(self.last_viewport.saturating_sub(PAGE_SCROLL_OVERLAP)).max(1)
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
					// Upstream keeps PAGE_SCROLL_OVERLAP lines of context
					// visible across a page turn (tui-alt-screen.ts:763-770).
					KeyCode::PageUp => {
						self.scroll_up(self.page_step());
						true
					}
					KeyCode::PageDown => {
						self.scroll_down(self.page_step());
						true
					}
					_ => false,
				}
			}
			Event::Mouse(mouse) => match mouse.kind {
				MouseEventKind::ScrollUp => {
					let lines = self.wheel.next(-1, wheel_now_ms());
					self.scroll_up(lines);
					true
				}
				MouseEventKind::ScrollDown => {
					let lines = self.wheel.next(1, wheel_now_ms());
					self.scroll_down(lines);
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
		self.last_viewport = viewport;

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

#[cfg(test)]
mod tests {
	use super::*;

	fn view_with_lines(count: usize) -> ScrollView {
		let mut view = ScrollView::new();
		view.set_lines(
			(0..count)
				.map(|i| Line::from(format!("line {i}")))
				.collect(),
		);
		view
	}

	fn render_at(view: &mut ScrollView, height: u16) {
		let area = Rect {
			x: 0,
			y: 0,
			width: 40,
			height,
		};
		let mut buf = Buffer::empty(area);
		view.render(area, &mut buf);
	}

	#[test]
	fn wheel_step_does_not_snap_to_bottom() {
		// Regression: scroll_down used to take the viewport as a parameter,
		// received usize::MAX, computed max_offset 0, and every wheel event
		// clamped straight to the bottom.
		let mut view = view_with_lines(100);
		render_at(&mut view, 10);
		view.scroll_up(5);
		assert_eq!(view.offset, 85);
		view.scroll_by(1);
		assert!(
			!view.follow_bottom,
			"mid-transcript scroll keeps follow off"
		);
		assert_eq!(view.offset, 86, "moved exactly one line, not to the bottom");
	}

	#[test]
	fn scrolling_back_to_bottom_rearms_follow() {
		let mut view = view_with_lines(100);
		render_at(&mut view, 10);
		view.scroll_up(15);
		assert!(!view.follow_bottom);
		view.scroll_down(15);
		assert!(view.follow_bottom, "landing at the end resumes tail-follow");
	}

	#[test]
	fn scroll_by_clamps_and_resolves_the_follow_sentinel() {
		let mut view = view_with_lines(100);
		render_at(&mut view, 10);
		view.scroll_up(1_000);
		assert_eq!(view.offset, 0, "clamped at the top");
		view.scroll_by(50);
		assert_eq!(view.offset, 50);
		// Sentinel arithmetic must not overflow while following.
		let mut view = view_with_lines(100);
		render_at(&mut view, 10);
		view.scroll_by(1);
		assert!(view.follow_bottom && view.offset <= 90);
	}

	#[test]
	fn page_step_keeps_context_lines_visible() {
		let mut view = view_with_lines(100);
		render_at(&mut view, 10);
		view.scroll_up(1_000); // to the top
		assert_eq!(view.offset, 0);
		// viewport 10 - overlap 4 = 6 lines per page; two pages land at 12.
		view.scroll_down(view.page_step());
		assert_eq!(view.offset, 6);
		view.scroll_down(view.page_step());
		assert_eq!(view.offset, 12);
	}
}
