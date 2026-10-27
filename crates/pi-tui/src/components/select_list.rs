//! Select list — port of `pi/packages/tui/src/components/select-list.ts`
//! plus the model-selector's centered-window rendering and `→` cursor.
//!
//! A filterable, keyboard-navigable list used by overlays (`/model` now,
//! sessions/settings later). Filtering is fuzzy over `value` + `label`.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// One selectable row.
#[derive(Debug, Clone)]
pub struct SelectItem {
	/// Identity returned on selection (e.g. "provider/model-id").
	pub value: String,
	/// Primary display text.
	pub label: String,
	/// Secondary text shown after the label.
	pub description: Option<String>,
}

/// What a key event means for the list's owner.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectAction {
	/// The user confirmed the item at this index into `filtered()`.
	Select(usize),
	/// Escape / ctrl+c.
	Cancel,
	/// Consumed (navigation/filter editing) but no decision yet.
	Handled,
	/// Not a list key; let the owner's overlay input handle it (e.g. typing
	/// goes through the overlay's search line first, then reaches us).
	Ignored,
}

pub struct SelectList {
	items: Vec<SelectItem>,
	filtered: Vec<usize>, // indices into items
	filter: String,
	selected: usize,
	max_visible: usize,
	/// Value of the "current" item, rendered with a `✓` marker.
	mark_value: Option<String>,
	no_match_message: String,
}

const PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;

impl SelectList {
	pub fn new(items: Vec<SelectItem>, max_visible: usize) -> Self {
		let filtered = (0..items.len()).collect();
		Self {
			items,
			filtered,
			filter: String::new(),
			selected: 0,
			max_visible: max_visible.max(1),
			mark_value: None,
			no_match_message: "No matching entries".into(),
		}
	}

	pub fn set_items(&mut self, items: Vec<SelectItem>) {
		self.items = items;
		self.refilter();
	}

	pub fn set_mark_value(&mut self, value: Option<String>) {
		self.mark_value = value;
	}

	pub fn set_no_match_message(&mut self, message: impl Into<String>) {
		self.no_match_message = message.into();
	}

	pub fn filter(&self) -> &str {
		&self.filter
	}

	pub fn set_filter(&mut self, filter: impl Into<String>) {
		self.filter = filter.into();
		// Fresh query moves the selector to the top row so the best fuzzy
		// match is highlighted; clearing the query keeps the position clamped.
		self.refilter();
		if !self.filter.is_empty() {
			self.selected = 0;
		} else {
			self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
		}
	}

	fn refilter(&mut self) {
		if self.filter.trim().is_empty() {
			self.filtered = (0..self.items.len()).collect();
			return;
		}
		let mut scored: Vec<(usize, f64)> = Vec::new();
		for (i, item) in self.items.iter().enumerate() {
			let text = match &item.description {
				Some(desc) => format!("{} {} {}", item.value, item.label, desc),
				None => format!("{} {}", item.value, item.label),
			};
			let m = crate::fuzzy::fuzzy_match(self.filter.trim(), &text);
			if m.matches {
				scored.push((i, m.score));
			}
		}
		scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
		self.filtered = scored.into_iter().map(|(i, _)| i).collect();
	}

	pub fn filtered(&self) -> Vec<&SelectItem> {
		self.filtered.iter().map(|&i| &self.items[i]).collect()
	}

	pub fn selected_item(&self) -> Option<&SelectItem> {
		self.filtered.get(self.selected).map(|&i| &self.items[i])
	}

	pub fn selected_index(&self) -> usize {
		self.selected
	}

	pub fn len(&self) -> usize {
		self.items.len()
	}

	pub fn is_empty(&self) -> bool {
		self.items.is_empty()
	}

	fn move_selection(&mut self, delta: isize) {
		if self.filtered.is_empty() {
			return;
		}
		let len = self.filtered.len() as isize;
		self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
	}

	/// Handle a key event; returns the resulting action.
	pub fn handle_key(&mut self, key: KeyEvent) -> SelectAction {
		if key.kind == KeyEventKind::Release {
			return SelectAction::Handled;
		}
		match (key.code, key.modifiers) {
			(KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
				self.move_selection(-1);
				SelectAction::Handled
			}
			(KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
				self.move_selection(1);
				SelectAction::Handled
			}
			(KeyCode::Enter, _) => SelectAction::Select(self.selected),
			(KeyCode::Esc, _) => SelectAction::Cancel,
			(KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => SelectAction::Cancel,
			_ => SelectAction::Ignored,
		}
	}

	/// Render the visible window of items at `width` cells.
	pub fn render_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
		if self.filtered.is_empty() {
			return vec![Line::from(Span::styled(
				format!("  {}", self.no_match_message),
				Style::new().fg(theme.muted),
			))];
		}

		let max_visible = self.max_visible;
		let start = (self.selected.saturating_sub(max_visible / 2))
			.min(self.filtered.len().saturating_sub(max_visible));
		let end = (start + max_visible).min(self.filtered.len());

		let mut lines = Vec::new();
		for &idx in &self.filtered[start..end] {
			let item = &self.items[idx];
			let is_selected = idx == self.filtered[self.selected];
			let is_marked = self.mark_value.as_deref() == Some(item.value.as_str());

			let cursor = if is_selected {
				Span::styled("→ ".to_string(), Style::new().fg(theme.accent))
			} else {
				Span::raw("  ".to_string())
			};
			let marker = if is_marked {
				Span::styled("✓ ".to_string(), Style::new().fg(theme.accent))
			} else {
				Span::raw("  ".to_string())
			};
			let label_style = if is_selected {
				Style::new().fg(theme.accent)
			} else {
				Style::default()
			};

			let mut spans = vec![cursor, marker];
			spans.push(Span::styled(
				truncate(&item.label, PRIMARY_COLUMN_WIDTH),
				label_style,
			));

			let mut desc_parts: Vec<String> = Vec::new();
			if let Some(desc) = &item.description {
				desc_parts.push(desc.clone());
			}
			if !desc_parts.is_empty() {
				let used = 4 + PRIMARY_COLUMN_WIDTH;
				let avail = width.saturating_sub(used + PRIMARY_COLUMN_GAP).max(10);
				let text = desc_parts.join(" ");
				if !text.is_empty() {
					spans.push(Span::raw(" ".repeat(PRIMARY_COLUMN_GAP)));
					spans.push(Span::styled(
						truncate(&text, avail),
						Style::new().fg(theme.muted),
					));
				}
			}
			lines.push(Line::from(spans));
		}

		if start > 0 || end < self.filtered.len() {
			lines.push(Line::from(Span::styled(
				format!("  ({}/{})", self.selected + 1, self.filtered.len()),
				Style::new().fg(theme.muted),
			)));
		}
		lines
	}
}

fn truncate(text: &str, max: usize) -> String {
	if UnicodeWidthStr::width(text) <= max {
		return text.to_string();
	}
	let mut out = String::new();
	let mut w = 0usize;
	for c in text.chars() {
		let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
		if w + cw > max.saturating_sub(1) {
			break;
		}
		out.push(c);
		w += cw;
	}
	out.push('…');
	out
}

/// Consume an `Event` for a SelectList (convenience wrapper).
pub fn select_list_event(list: &mut SelectList, event: &Event) -> SelectAction {
	match event {
		Event::Key(key) => list.handle_key(*key),
		_ => SelectAction::Ignored,
	}
}
