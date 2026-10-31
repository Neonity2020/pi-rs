//! Tree selector — port of
//! `pi/packages/coding-agent/src/modes/interactive/components/tree-selector.ts`
//! (the `TreeList` core), reduced to what pi-rs interactive mode drives:
//! tree-shaped display with ASCII connectors, the active-leaf path marker,
//! the five filter modes, incremental search, and page navigation.
//!
//! Deviations from upstream (declared): folding (`app.tree.foldOrUp`), label
//! editing and label timestamps, clipboard copy, and the horizontally
//! panning viewport are not ported — rows clip at the overlay width. The
//! neutral [`TreeItem`] input keeps this crate free of session-model types;
//! the caller maps its entries onto it.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::keybindings::KeybindingsManager;
use crate::theme::Theme;

/// Filter modes (`FilterMode` upstream). `Default` hides bookkeeping entries
/// and text-less assistant messages; the others widen or narrow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeFilterMode {
	Default,
	NoTools,
	UserOnly,
	LabeledOnly,
	All,
}

impl TreeFilterMode {
	/// Cycle order (`app.tree.filter.cycleForward`).
	pub const ALL: [TreeFilterMode; 5] = [
		TreeFilterMode::Default,
		TreeFilterMode::NoTools,
		TreeFilterMode::UserOnly,
		TreeFilterMode::LabeledOnly,
		TreeFilterMode::All,
	];

	/// Parse a normalized settings value (`normalize_tree_filter_mode` result).
	pub fn parse(value: &str) -> Self {
		match value {
			"no-tools" => TreeFilterMode::NoTools,
			"user-only" => TreeFilterMode::UserOnly,
			"labeled-only" => TreeFilterMode::LabeledOnly,
			"all" => TreeFilterMode::All,
			_ => TreeFilterMode::Default,
		}
	}

	/// Status-line suffix (`getStatusLabels` upstream).
	pub fn label(self) -> &'static str {
		match self {
			TreeFilterMode::Default => "",
			TreeFilterMode::NoTools => " [no-tools]",
			TreeFilterMode::UserOnly => " [user]",
			TreeFilterMode::LabeledOnly => " [labeled]",
			TreeFilterMode::All => " [all]",
		}
	}

	fn cycle(self, forward: bool) -> Self {
		let index = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
		let next = if forward {
			(index + 1) % Self::ALL.len()
		} else {
			(index + Self::ALL.len() - 1) % Self::ALL.len()
		};
		Self::ALL[next]
	}
}

/// Entry category driving the filter modes (upstream's `entry.type` /
/// `message.role` checks in `applyFilter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeCategory {
	/// A `user` message.
	UserMessage,
	/// An `assistant` message; `notable` marks error/aborted messages, which
	/// stay visible even without text.
	AssistantMessage { has_text: bool, notable: bool },
	/// A `toolResult` message (hidden by `no-tools`).
	ToolResult,
	/// A `bashExecution` message.
	BashExecution,
	/// A `compaction` entry (default view keeps it visible).
	Compaction,
	/// A `branch_summary` entry (default view keeps it visible).
	BranchSummary,
	/// Entries the default view always shows: custom messages.
	Always,
	/// Bookkeeping entries (`label`/`context_edit`/`custom`/`model_change`/
	/// `thinking_level_change`/`session_info`) — hidden unless `all`.
	Settings,
	/// Never shown (`usage` and other payload-less entries).
	Hidden,
}

/// One neutral tree row; the caller maps its model onto this.
#[derive(Debug, Clone)]
pub struct TreeItem {
	/// Entry id returned by [`TreeAction::Select`].
	pub id: String,
	pub parent_id: Option<String>,
	pub category: TreeCategory,
	/// Badge rendered as `[badge] ` before the content (a resolved label).
	pub badge: Option<String>,
	/// Role/kind prefix such as `user: ` (colored by category at render time).
	pub prefix: String,
	/// Single-line display text after the prefix.
	pub text: String,
	/// Extra terms the search matches beyond prefix + text.
	pub search_extra: String,
}

impl TreeItem {
	fn searchable(&self) -> String {
		format!("{} {} {}", self.prefix, self.text, self.search_extra).to_lowercase()
	}
}

/// What a key event means for the selector's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeAction {
	/// The user confirmed the entry with this id.
	Select(String),
	/// Escape with an empty search.
	Cancel,
	/// Consumed (navigation/filter/search editing).
	Handled,
	/// Not a tree key.
	Ignored,
}

/// DFS work item: the node with its inherited layout
/// (`flattenTree`'s stack upstream).
type StackItem = (
	usize,
	usize,
	bool,
	Option<bool>,
	bool,
	Vec<(usize, bool)>,
	bool,
);

/// One visible row's computed layout. Indent/connector/gutter rules mirror
/// upstream `flattenTree`/`recalculateVisualStructure`: a branching parent
/// shifts children one level right, single-child chains stay flat, connectors
/// (`├─`/`└─`) appear only where a parent branches, and ancestor branch
/// points leave `│` gutters unless that ancestor was the last sibling.
struct VisibleRow {
	index: usize,
	indent: usize,
	/// `Some(is_last)` → draw the connector at `indent - 1`.
	connector: Option<bool>,
	/// `(level, show)` gutters inherited from ancestor branch points.
	gutters: Vec<(usize, bool)>,
	on_active_path: bool,
}

/// The tree selector state. Rebuilds the visible structure whenever the
/// filter mode or search changes, keeping the cursor on the nearest visible
/// ancestor of the previous selection (upstream `findNearestVisibleIndex`).
pub struct TreeSelector {
	items: Vec<TreeItem>,
	parent_of: HashMap<String, Option<String>>,
	current_leaf: Option<String>,
	filter_mode: TreeFilterMode,
	search: String,
	selected_id: Option<String>,
	rows: Vec<VisibleRow>,
	/// Index into `rows` of the selection.
	selected: usize,
}

impl TreeSelector {
	pub fn new(
		items: Vec<TreeItem>,
		current_leaf: Option<&str>,
		initial_selected_id: Option<&str>,
		initial_filter_mode: TreeFilterMode,
	) -> Self {
		let parent_of = items
			.iter()
			.map(|item| (item.id.clone(), item.parent_id.clone()))
			.collect();
		let current_leaf = current_leaf.map(str::to_owned);
		let selected_id = initial_selected_id
			.map(str::to_owned)
			.or_else(|| current_leaf.clone());
		let mut selector = Self {
			items,
			parent_of,
			current_leaf,
			filter_mode: initial_filter_mode,
			search: String::new(),
			selected_id,
			rows: Vec::new(),
			selected: 0,
		};
		selector.rebuild();
		selector
	}

	pub fn filter_mode(&self) -> TreeFilterMode {
		self.filter_mode
	}

	pub fn search(&self) -> &str {
		&self.search
	}

	/// The selected entry id (`None` when the visible list is empty).
	pub fn selected_id(&self) -> Option<&str> {
		self.rows
			.get(self.selected)
			.map(|row| self.items[row.index].id.as_str())
	}

	pub fn len(&self) -> usize {
		self.rows.len()
	}

	pub fn is_empty(&self) -> bool {
		self.rows.is_empty()
	}

	/// Whether the entry passes the current mode + search filters
	/// (`applyFilter` upstream). Pure so the mode semantics are testable.
	fn passes_filter(&self, item: &TreeItem, search_tokens: &[String]) -> bool {
		let is_current_leaf = Some(item.id.as_str()) == self.current_leaf.as_deref();
		match item.category {
			TreeCategory::Hidden => return false,
			// Text-less assistant responses stay hidden unless they are the
			// current leaf or carry an error/abort note.
			TreeCategory::AssistantMessage {
				has_text: false,
				notable: false,
			} if !is_current_leaf => {
				return false;
			}
			_ => {}
		}
		let passes = match self.filter_mode {
			TreeFilterMode::UserOnly => item.category == TreeCategory::UserMessage,
			TreeFilterMode::NoTools => !matches!(
				item.category,
				TreeCategory::ToolResult | TreeCategory::Settings
			),
			TreeFilterMode::LabeledOnly => item.badge.is_some(),
			TreeFilterMode::All => true,
			TreeFilterMode::Default => item.category != TreeCategory::Settings,
		};
		if !passes {
			return false;
		}
		search_tokens
			.iter()
			.all(|token| item.searchable().contains(token))
	}

	/// Recompute the visible rows: filter, re-attach entries whose ancestors
	/// were filtered out to their nearest visible ancestor, then lay out
	/// indent/connectors with the active-leaf branch sorted first (upstream
	/// `flattenTree` ordering).
	fn rebuild(&mut self) {
		let search_tokens: Vec<String> = self
			.search
			.to_lowercase()
			.split_whitespace()
			.map(str::to_owned)
			.collect();
		let visible: Vec<usize> = self
			.items
			.iter()
			.enumerate()
			.filter(|(_, item)| self.passes_filter(item, &search_tokens))
			.map(|(index, _)| index)
			.collect();
		let index_by_id: HashMap<&str, usize> = visible
			.iter()
			.map(|&index| (self.items[index].id.as_str(), index))
			.collect();

		// Nearest visible ancestor per visible item (`visibleParent` upstream).
		let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
		for &index in &visible {
			let mapped = self.nearest_visible_ancestor(index, &index_by_id);
			children.entry(mapped).or_default().push(index);
		}

		// Subtree containment of the active leaf, so the current branch sorts
		// first at every branch point (`containsActive` upstream).
		let mut contains_leaf: HashMap<usize, bool> = HashMap::new();
		for &index in visible.iter().rev() {
			let mut has = self.current_leaf.as_deref() == Some(self.items[index].id.as_str());
			if let Some(kids) = children.get(&Some(index)) {
				for &child in kids {
					has = has || contains_leaf.get(&child).copied().unwrap_or(false);
				}
			}
			contains_leaf.insert(index, has);
		}

		let multiple_roots = children.get(&None).is_some_and(|roots| roots.len() > 1);

		// Active path = the id chain from the current leaf up to its root
		// (`buildActivePath` upstream) — a row is marked when its id is on it.
		let mut active_path: HashSet<String> = HashSet::new();
		let mut walk = self.current_leaf.clone();
		while let Some(id) = walk {
			active_path.insert(id.clone());
			walk = self.parent_of.get(&id).cloned().flatten();
		}

		// Iterative DFS; the stack holds reverse-ordered children so the pop
		// order matches the forward traversal.
		let mut rows: Vec<VisibleRow> = Vec::new();
		// (index, indent, just_branched, connector, is_last, gutters, virtual_root_child)
		let mut stack: Vec<StackItem> = Vec::new();
		let mut roots = children.get(&None).cloned().unwrap_or_default();
		roots.sort_by_key(|&index| !contains_leaf.get(&index).copied().unwrap_or(false));
		for (position, &index) in roots.iter().enumerate().rev() {
			let is_last = position + 1 == roots.len();
			stack.push((
				index,
				if multiple_roots { 1 } else { 0 },
				multiple_roots,
				if multiple_roots { Some(is_last) } else { None },
				is_last,
				Vec::new(),
				multiple_roots,
			));
		}
		while let Some((index, indent, just_branched, connector, is_last, gutters, virtual_root)) =
			stack.pop()
		{
			rows.push(VisibleRow {
				index,
				// With several roots the virtual root absorbs one level.
				indent: if multiple_roots {
					indent.saturating_sub(1)
				} else {
					indent
				},
				// The virtual root's children hide their connector
				// (`isVirtualRootChild` upstream); deeper rows show theirs.
				connector: if virtual_root { None } else { connector },
				gutters: gutters.clone(),
				on_active_path: active_path.contains(&self.items[index].id),
			});
			let mut ordered = children.get(&Some(index)).cloned().unwrap_or_default();
			ordered.sort_by_key(|&child| !contains_leaf.get(&child).copied().unwrap_or(false));
			let multiple_children = ordered.len() > 1;
			let child_indent = if multiple_children || (just_branched && indent > 0) {
				indent + 1
			} else {
				indent
			};
			// A displayed connector leaves a gutter at its level for the
			// descendants; the gutter disappears once the branch point was the
			// last sibling (`show: !isLast` upstream). Gutter levels are only
			// tracked for the single-root layout.
			let mut child_gutters = gutters;
			if connector.is_some() && !multiple_roots && !virtual_root {
				child_gutters.push((indent.saturating_sub(1), !is_last));
			}
			for (position, &child) in ordered.iter().enumerate().rev() {
				let child_is_last = position + 1 == ordered.len();
				stack.push((
					child,
					child_indent,
					multiple_children,
					if multiple_children {
						Some(child_is_last)
					} else {
						connector
					},
					child_is_last,
					child_gutters.clone(),
					false,
				));
			}
		}

		self.selected = self.nearest_visible(&rows, self.selected_id.as_deref());
		self.rows = rows;
		if let Some(row) = self.rows.get(self.selected) {
			self.selected_id = Some(self.items[row.index].id.clone());
		}
	}

	/// Walks the full parent chain to the closest visible ancestor
	/// (`findVisibleAncestor` upstream); `None` when the chain reaches a root.
	fn nearest_visible_ancestor(
		&self,
		index: usize,
		index_by_id: &HashMap<&str, usize>,
	) -> Option<usize> {
		let mut ancestor = self.items[index].parent_id.clone()?;
		loop {
			if let Some(&parent) = index_by_id.get(ancestor.as_str()) {
				return Some(parent);
			}
			ancestor = self.parent_of.get(&ancestor).cloned().flatten()?;
		}
	}

	/// Index into `rows` of the entry or its nearest visible ancestor
	/// (`findNearestVisibleIndex` upstream); falls back to the last row.
	fn nearest_visible(&self, rows: &[VisibleRow], target: Option<&str>) -> usize {
		if rows.is_empty() {
			return 0;
		}
		let Some(mut id) = target.map(str::to_owned) else {
			return rows.len() - 1;
		};
		let position_by_id: HashMap<&str, usize> = rows
			.iter()
			.enumerate()
			.map(|(position, row)| (self.items[row.index].id.as_str(), position))
			.collect();
		loop {
			if let Some(&position) = position_by_id.get(id.as_str()) {
				return position;
			}
			match self.parent_of.get(&id).cloned().flatten() {
				Some(parent) => id = parent,
				None => return rows.len() - 1,
			}
		}
	}

	fn move_selection(&mut self, delta: isize) {
		if self.rows.is_empty() {
			return;
		}
		let len = self.rows.len() as isize;
		self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
		self.selected_id = Some(self.items[self.rows[self.selected].index].id.clone());
	}

	fn page(&mut self, delta: isize, max_visible: usize) {
		if self.rows.is_empty() {
			return;
		}
		let next = (self.selected as isize + delta * max_visible as isize)
			.clamp(0, self.rows.len() as isize - 1);
		self.selected = next as usize;
		self.selected_id = Some(self.items[self.rows[self.selected].index].id.clone());
	}

	/// Handle a key through the resolved bindings (upstream asks the
	/// keybindings manager for `tui.select.*` / `app.tree.filter.*`). Search
	/// characters append directly; backspace pops.
	pub fn handle_key(&mut self, key: &KeyEvent, keybindings: &KeybindingsManager) -> TreeAction {
		if key.kind == KeyEventKind::Release {
			return TreeAction::Handled;
		}
		if keybindings.matches("tui.select.up", key) {
			self.move_selection(-1);
			return TreeAction::Handled;
		}
		if keybindings.matches("tui.select.down", key) {
			self.move_selection(1);
			return TreeAction::Handled;
		}
		if keybindings.matches("tui.select.confirm", key) {
			return match self.selected_id() {
				Some(id) => TreeAction::Select(id.to_owned()),
				None => TreeAction::Handled,
			};
		}
		if keybindings.matches("tui.select.cancel", key) {
			if !self.search.is_empty() {
				self.search.clear();
				self.rebuild();
				return TreeAction::Handled;
			}
			return TreeAction::Cancel;
		}
		if keybindings.matches("tui.editor.cursorLeft", key) {
			self.page(-1, 10);
			return TreeAction::Handled;
		}
		if keybindings.matches("tui.editor.cursorRight", key) {
			self.page(1, 10);
			return TreeAction::Handled;
		}
		let toggled = if keybindings.matches("app.tree.filter.default", key) {
			Some(TreeFilterMode::Default)
		} else if keybindings.matches("app.tree.filter.noTools", key) {
			Some(if self.filter_mode == TreeFilterMode::NoTools {
				TreeFilterMode::Default
			} else {
				TreeFilterMode::NoTools
			})
		} else if keybindings.matches("app.tree.filter.userOnly", key) {
			Some(if self.filter_mode == TreeFilterMode::UserOnly {
				TreeFilterMode::Default
			} else {
				TreeFilterMode::UserOnly
			})
		} else if keybindings.matches("app.tree.filter.labeledOnly", key) {
			Some(if self.filter_mode == TreeFilterMode::LabeledOnly {
				TreeFilterMode::Default
			} else {
				TreeFilterMode::LabeledOnly
			})
		} else if keybindings.matches("app.tree.filter.all", key) {
			Some(if self.filter_mode == TreeFilterMode::All {
				TreeFilterMode::Default
			} else {
				TreeFilterMode::All
			})
		} else if keybindings.matches("app.tree.filter.cycleForward", key) {
			Some(self.filter_mode.cycle(true))
		} else if keybindings.matches("app.tree.filter.cycleBackward", key) {
			Some(self.filter_mode.cycle(false))
		} else {
			None
		};
		if let Some(mode) = toggled {
			self.filter_mode = mode;
			self.search.clear();
			self.rebuild();
			return TreeAction::Handled;
		}
		let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
		match key.code {
			KeyCode::Char(c) if plain && !c.is_control() => {
				self.search.push(c);
				self.rebuild();
				TreeAction::Handled
			}
			KeyCode::Backspace if plain => {
				if self.search.pop().is_some() {
					self.rebuild();
				}
				TreeAction::Handled
			}
			_ => TreeAction::Ignored,
		}
	}

	/// Render the visible window at `width` cells; `max_visible` sizes the
	/// window. The final line is the `(n/total)` counter with the filter
	/// label (upstream `render`'s status line).
	pub fn render_lines(
		&self,
		width: usize,
		theme: &Theme,
		max_visible: usize,
	) -> Vec<Line<'static>> {
		if self.rows.is_empty() {
			return vec![
				Line::from(Span::styled(
					"  No entries found",
					Style::new().fg(theme.muted),
				)),
				Line::from(Span::styled(
					format!("  (0/0){}", self.filter_mode.label()),
					Style::new().fg(theme.muted),
				)),
			];
		}
		let max_visible = max_visible.max(1);
		let start = self
			.selected
			.saturating_sub(max_visible / 2)
			.min(self.rows.len().saturating_sub(max_visible));
		let end = (start + max_visible).min(self.rows.len());

		let mut lines = Vec::new();
		for position in start..end {
			let row = &self.rows[position];
			let item = &self.items[row.index];
			let is_selected = position == self.selected;

			let cursor = if is_selected {
				Span::styled("› ", Style::new().fg(theme.accent))
			} else {
				Span::raw("  ".to_string())
			};

			// Prefix: 3 chars per indent level; gutters (│ / space) sit at
			// ancestor branch levels, the connector one level out.
			let mut prefix = String::new();
			let connector_level = row.connector.map(|_| row.indent.saturating_sub(1));
			for level in 0..row.indent {
				if connector_level == Some(level) {
					prefix.push_str(if row.connector == Some(true) {
						"└─ "
					} else {
						"├─ "
					});
					continue;
				}
				let show_gutter = row
					.gutters
					.iter()
					.filter(|(gutter_level, _)| *gutter_level == level)
					.map(|(_, show)| *show)
					.next_back()
					.unwrap_or(false);
				prefix.push_str(if show_gutter { "│  " } else { "   " });
			}

			let mut spans = vec![cursor];
			if !prefix.is_empty() {
				spans.push(Span::styled(
					prefix,
					if is_selected {
						Style::new().fg(theme.accent)
					} else {
						Style::new().fg(theme.muted)
					},
				));
			}
			if row.on_active_path {
				spans.push(Span::styled("• ", Style::new().fg(theme.accent)));
			}
			if let Some(badge) = &item.badge {
				spans.push(Span::styled(
					format!("[{badge}] "),
					Style::new().fg(theme.tool),
				));
			}
			let (prefix_style, text_style) = match item.category {
				TreeCategory::UserMessage => (Style::new().fg(theme.accent), Style::default()),
				TreeCategory::AssistantMessage { .. } => {
					(Style::new().fg(theme.success), Style::default())
				}
				TreeCategory::ToolResult | TreeCategory::BashExecution | TreeCategory::Settings => {
					(Style::new().fg(theme.muted), Style::new().fg(theme.muted))
				}
				TreeCategory::Compaction => {
					(Style::new().fg(theme.border), Style::new().fg(theme.border))
				}
				TreeCategory::BranchSummary => (Style::new().fg(theme.tool), Style::default()),
				TreeCategory::Always => (Style::new().fg(theme.muted), Style::default()),
				TreeCategory::Hidden => {
					(Style::new().fg(theme.muted), Style::new().fg(theme.muted))
				}
			};
			let prefix_style = if is_selected {
				prefix_style.add_modifier(Modifier::BOLD)
			} else {
				prefix_style
			};
			let text_style = if is_selected {
				text_style.add_modifier(Modifier::BOLD)
			} else {
				text_style
			};
			if !item.prefix.is_empty() {
				spans.push(Span::styled(item.prefix.clone(), prefix_style));
			}
			spans.push(Span::styled(single_line(&item.text), text_style));

			// Clip to the overlay width (upstream pans horizontally instead).
			let mut used = 0usize;
			let mut clipped: Vec<Span> = Vec::new();
			for span in spans {
				if used >= width {
					break;
				}
				let span_width = UnicodeWidthStr::width(span.content.as_ref());
				if used + span_width > width {
					clipped.push(Span::styled(
						truncate_to_width(span.content.as_ref(), width - used),
						span.style,
					));
					break;
				}
				used += span_width;
				clipped.push(span);
			}
			lines.push(Line::from(clipped));
		}
		lines.push(Line::from(Span::styled(
			format!(
				"  ({}/{}){}",
				self.selected + 1,
				self.rows.len(),
				self.filter_mode.label()
			),
			Style::new().fg(theme.muted),
		)));
		lines
	}
}

fn single_line(text: &str) -> String {
	text.chars()
		.map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
		.collect()
}

fn truncate_to_width(text: &str, max: usize) -> String {
	if UnicodeWidthStr::width(text) <= max {
		return text.to_string();
	}
	let mut out = String::new();
	let mut used = 0usize;
	for c in text.chars() {
		let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
		if used + width >= max.saturating_sub(1) {
			break;
		}
		out.push(c);
		used += width;
	}
	out.push('…');
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn item(id: &str, parent: Option<&str>, category: TreeCategory) -> TreeItem {
		TreeItem {
			id: id.to_string(),
			parent_id: parent.map(str::to_owned),
			category,
			badge: None,
			prefix: String::new(),
			text: format!("entry {id}"),
			search_extra: String::new(),
		}
	}

	fn selector(items: Vec<TreeItem>, leaf: Option<&str>) -> TreeSelector {
		TreeSelector::new(items, leaf, None, TreeFilterMode::Default)
	}

	fn ids(selector: &TreeSelector) -> Vec<String> {
		selector
			.rows
			.iter()
			.map(|row| selector.items[row.index].id.clone())
			.collect()
	}

	#[test]
	fn filter_modes_hide_and_show_categories() {
		let mut selector = selector(
			vec![
				item("u", None, TreeCategory::UserMessage),
				item(
					"a",
					Some("u"),
					TreeCategory::AssistantMessage {
						has_text: true,
						notable: false,
					},
				),
				item("t", Some("a"), TreeCategory::ToolResult),
				item("s", Some("t"), TreeCategory::Settings),
				item("c", Some("s"), TreeCategory::Compaction),
				item("x", Some("c"), TreeCategory::Hidden),
			],
			Some("c"),
		);
		assert_eq!(ids(&selector), vec!["u", "a", "t", "c"]);
		selector.filter_mode = TreeFilterMode::NoTools;
		selector.rebuild();
		assert_eq!(ids(&selector), vec!["u", "a", "c"]);
		selector.filter_mode = TreeFilterMode::UserOnly;
		selector.rebuild();
		assert_eq!(ids(&selector), vec!["u"]);
		selector.filter_mode = TreeFilterMode::All;
		selector.rebuild();
		assert_eq!(ids(&selector), vec!["u", "a", "t", "s", "c"]);
	}

	#[test]
	fn textless_assistant_hides_unless_leaf_or_notable() {
		let selector = selector(
			vec![
				item("u", None, TreeCategory::UserMessage),
				item(
					"a",
					Some("u"),
					TreeCategory::AssistantMessage {
						has_text: false,
						notable: false,
					},
				),
				item(
					"e",
					Some("a"),
					TreeCategory::AssistantMessage {
						has_text: false,
						notable: true,
					},
				),
			],
			Some("e"),
		);
		assert_eq!(ids(&selector), vec!["u", "e"]);
	}

	#[test]
	fn search_filters_and_selection_walks_to_visible_ancestor() {
		let mut selector = selector(
			vec![
				item("u", None, TreeCategory::UserMessage),
				item(
					"a",
					Some("u"),
					TreeCategory::AssistantMessage {
						has_text: true,
						notable: false,
					},
				),
				item("t", Some("a"), TreeCategory::ToolResult),
			],
			Some("t"),
		);
		selector.selected_id = Some("t".into());
		selector.search = "entry u".into();
		selector.rebuild();
		assert_eq!(ids(&selector), vec!["u"]);
		// The cursor walked up from the hidden tool result to the visible user
		// message (findNearestVisibleIndex).
		assert_eq!(selector.selected_id(), Some("u"));
	}

	#[test]
	fn labeled_only_requires_a_badge() {
		let mut labeled = item("u", None, TreeCategory::UserMessage);
		labeled.badge = Some("keep".into());
		let mut selector = selector(
			vec![labeled, item("v", Some("u"), TreeCategory::UserMessage)],
			Some("v"),
		);
		selector.filter_mode = TreeFilterMode::LabeledOnly;
		selector.rebuild();
		assert_eq!(ids(&selector), vec!["u"]);
	}

	#[test]
	fn filter_mode_cycles_and_parses() {
		assert_eq!(TreeFilterMode::parse("user-only"), TreeFilterMode::UserOnly);
		assert_eq!(TreeFilterMode::parse("bogus"), TreeFilterMode::Default);
		assert_eq!(TreeFilterMode::Default.cycle(true), TreeFilterMode::NoTools);
		assert_eq!(TreeFilterMode::Default.cycle(false), TreeFilterMode::All);
		assert_eq!(TreeFilterMode::All.cycle(true), TreeFilterMode::Default);
	}

	#[test]
	fn branch_children_indent_and_current_branch_sorts_first() {
		// u → a1 → (b1, a2): the active path u→a1→a2 must render before b1 and
		// both branch children gain one indent level with connectors.
		let selector = selector(
			vec![
				item("u", None, TreeCategory::UserMessage),
				item(
					"a1",
					Some("u"),
					TreeCategory::AssistantMessage {
						has_text: true,
						notable: false,
					},
				),
				item(
					"b1",
					Some("a1"),
					TreeCategory::AssistantMessage {
						has_text: true,
						notable: false,
					},
				),
				item(
					"a2",
					Some("a1"),
					TreeCategory::AssistantMessage {
						has_text: true,
						notable: false,
					},
				),
			],
			Some("a2"),
		);
		assert_eq!(ids(&selector), vec!["u", "a1", "a2", "b1"]);
		let indents: Vec<usize> = selector.rows.iter().map(|row| row.indent).collect();
		assert_eq!(indents, vec![0, 0, 1, 1]);
		let connectors: Vec<Option<bool>> = selector.rows.iter().map(|row| row.connector).collect();
		assert_eq!(connectors, vec![None, None, Some(false), Some(true)]);
		// Active path markers on u, a1, a2 only.
		let active: Vec<bool> = selector.rows.iter().map(|row| row.on_active_path).collect();
		assert_eq!(active, vec![true, true, true, false]);
	}

	#[test]
	fn single_child_chain_stays_flat() {
		let selector = selector(
			vec![
				item("u", None, TreeCategory::UserMessage),
				item(
					"a",
					Some("u"),
					TreeCategory::AssistantMessage {
						has_text: true,
						notable: false,
					},
				),
				item("t", Some("a"), TreeCategory::ToolResult),
			],
			Some("t"),
		);
		let indents: Vec<usize> = selector.rows.iter().map(|row| row.indent).collect();
		assert_eq!(indents, vec![0, 0, 0]);
		assert!(selector.rows.iter().all(|row| row.connector.is_none()));
	}
}
