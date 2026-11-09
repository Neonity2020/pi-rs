//! `pi-rs config` — the package-resource enable/disable selector, a port of
//! `pi/packages/coding-agent/src/modes/interactive/components/config-selector.ts`
//! (951 lines) reduced to the package-origin resources pi-rs models:
//!
//! - Two write scopes (`Tab` switches when the project is trusted): **Global**
//!   edits `<agent-dir>/settings.json`, **Project** edits `.pi/settings.json`
//!   overrides.
//! - Items group per package, then per resource type (Extensions / Skills /
//!   Prompts / Themes), sorted like upstream `buildGroups` (project packages
//!   first, then user; subgroups in type order; items by display name).
//! - Space toggles. Global mode writes `+rel`/`-rel` into the package entry.
//!   Project mode cycles **inherit → load/unload → inherit** (upstream
//!   `getNextOverrideState`) into the project entry (created as an
//!   `autoload:false` override shell when missing).
//! - Typing filters (case-insensitive substring over display name, type, and
//!   path, like upstream `filterItems`); ↑/↓ step between items,
//!   PgUp/PgDN page, Esc closes, ctrl+c exits 0.
//!
//! Declared reductions: top-level (non-package) resources are not listed
//! (their per-type settings arrays are not modeled); the terminal is ratatui,
//! not the pi-tui component tree, so chrome differs visually but carries the
//! same information. Writes take effect on the next `pi-rs` start — the
//! selector does not hot-reload a running session.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::core::package_manager::{PackageManager, ResourceItem, ResourceType};

/// Which settings file space edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteScope {
	Global,
	Project,
}

/// One rendered list row — groups and subgroups never take the cursor
/// (upstream `FlatEntry`).
#[derive(Debug, Clone)]
enum Row {
	Group {
		label: String,
		/// User-scope package in project mode (upstream "inherited global").
		inherited: bool,
	},
	Subgroup {
		label: &'static str,
		inherited: bool,
	},
	Item {
		index: usize,
	},
}

pub struct ConfigSelector {
	manager: PackageManager,
	agent_dir: PathBuf,
	write_scope: WriteScope,
	project_mode_available: bool,
	/// Freshly resolved on every toggle — the settings files are the single
	/// source of truth.
	items: Vec<ResourceItem>,
	rows: Vec<Row>,
	filtered: Vec<usize>,
	selected: usize,
	search: String,
	status: Option<String>,
	/// Terminal rows at open — the list viewport derives from it like
	/// upstream `maxVisible` (8 lines of chrome).
	terminal_height: u16,
}

impl ConfigSelector {
	pub fn new(
		manager: PackageManager,
		agent_dir: PathBuf,
		start_local: bool,
		project_mode_available: bool,
		terminal_height: u16,
	) -> Self {
		let mut selector = Self {
			manager,
			agent_dir,
			write_scope: if start_local {
				WriteScope::Project
			} else {
				WriteScope::Global
			},
			project_mode_available,
			items: Vec::new(),
			rows: Vec::new(),
			filtered: Vec::new(),
			selected: 0,
			search: String::new(),
			status: None,
			terminal_height: terminal_height.max(12),
		};
		selector.reload();
		selector
	}

	/// Re-resolve from disk and rebuild the row list.
	fn reload(&mut self) {
		let (items, _warnings) = self.manager.resolve_items();
		self.items = items;
		self.build_rows();
		self.apply_filter();
	}

	/// Scope-visible items: the global view hides project-only packages
	/// (upstream's global resolve covers global settings only).
	fn visible_items(&self) -> Vec<&ResourceItem> {
		match self.write_scope {
			WriteScope::Global => self
				.items
				.iter()
				.filter(|item| item.package_scope == "user")
				.collect(),
			WriteScope::Project => self.items.iter().collect(),
		}
	}

	/// Build the group/subgroup/item rows in upstream `buildGroups` order.
	fn build_rows(&mut self) {
		let project_mode = self.write_scope == WriteScope::Project;
		let mut items: Vec<usize> = (0..self.items.len()).collect();
		// Project packages first, then user (upstream's group sort); items
		// keep their resolve order (per package, per type) with subgroups in
		// the type order ResourceType::ALL already provides.
		items.sort_by(|a, b| {
			let (a, b) = (&self.items[*a], &self.items[*b]);
			let scope = |item: &ResourceItem| usize::from(item.package_scope == "user");
			scope(a)
				.cmp(&scope(b))
				.then(a.package_source.cmp(&b.package_source))
		});
		let mut rows = Vec::new();
		let mut last_group: Option<String> = None;
		let mut last_type: Option<ResourceType> = None;
		for index in items {
			let item = &self.items[index];
			let group_key = format!("{}:{}", item.package_scope, item.package_source);
			if last_group.as_ref() != Some(&group_key) {
				let inherited = project_mode && item.package_scope == "user";
				let label = if item.package_scope == "user" {
					format!(
						"{} (~/…/{})",
						short_source(&item.package_source),
						self.agent_dir
							.file_name()
							.map(|name| name.to_string_lossy().to_string())
							.unwrap_or_else(|| "agent".to_string())
					)
				} else {
					format!("{} (.pi)", short_source(&item.package_source))
				};
				rows.push(Row::Group { label, inherited });
				last_group = Some(group_key);
				last_type = None;
			}
			if last_type != Some(item.resource_type) {
				rows.push(Row::Subgroup {
					label: type_label(item.resource_type),
					inherited: project_mode && item.package_scope == "user",
				});
				last_type = Some(item.resource_type);
			}
			rows.push(Row::Item { index });
		}
		self.rows = rows;
	}

	fn apply_filter(&mut self) {
		let query = self.search.trim().to_lowercase();
		let filtered = if query.is_empty() {
			(0..self.rows.len()).collect()
		} else {
			// Items match on display name, type, or path; their group and
			// subgroup rows ride along (upstream filterItems).
			let item_matches: Vec<bool> = self
				.items
				.iter()
				.map(|item| {
					display_name(item).to_lowercase().contains(&query)
						|| item.resource_type.as_str().contains(&query)
						|| item.rel.to_lowercase().contains(&query)
				})
				.collect();
			let mut filtered = Vec::new();
			let mut current_group: Option<usize> = None;
			let mut current_subgroup: Option<usize> = None;
			let mut group_emitted = false;
			let mut subgroup_emitted = false;
			for position in 0..self.rows.len() {
				match &self.rows[position] {
					Row::Group { .. } => {
						current_group = Some(position);
						group_emitted = false;
					}
					Row::Subgroup { .. } => {
						current_subgroup = Some(position);
						subgroup_emitted = false;
					}
					Row::Item { index } if item_matches[*index] => {
						if let Some(group) = current_group
							&& !group_emitted
						{
							filtered.push(group);
							group_emitted = true;
						}
						if let Some(subgroup) = current_subgroup
							&& !subgroup_emitted
						{
							filtered.push(subgroup);
							subgroup_emitted = true;
						}
						filtered.push(position);
					}
					Row::Item { .. } => {}
				}
			}
			filtered
		};
		self.filtered = filtered;
		self.select_first_item();
	}

	fn select_first_item(&mut self) {
		self.selected = self
			.filtered
			.iter()
			.position(|&position| matches!(self.rows[position], Row::Item { .. }))
			.unwrap_or(0);
	}

	fn step_item(&mut self, direction: i32) {
		if self.filtered.is_empty() {
			return;
		}
		let mut position = self.selected;
		loop {
			let next = position as i32 + direction;
			if next < 0 || next as usize >= self.filtered.len() {
				return;
			}
			position = next as usize;
			if matches!(self.rows[self.filtered[position]], Row::Item { .. }) {
				self.selected = position;
				return;
			}
		}
	}

	fn page(&mut self, direction: i32) {
		let max_visible = self.max_visible() as i32;
		if max_visible <= 0 || self.filtered.is_empty() {
			return;
		}
		let mut target = (self.selected as i32 + direction * max_visible)
			.clamp(0, (self.filtered.len() - 1) as i32) as usize;
		while target < self.filtered.len()
			&& !matches!(self.rows[self.filtered[target]], Row::Item { .. })
		{
			if direction > 0 {
				target += 1;
			} else {
				target = target.saturating_sub(1);
			}
		}
		if target < self.filtered.len() {
			self.selected = target;
		}
	}

	fn max_visible(&self) -> u16 {
		// Upstream: terminalHeight - 8 lines of chrome (borders, header,
		// search); the ratatui block adds a bottom hint line, hence 10.
		self.terminal_height.saturating_sub(10).max(5)
	}

	fn switch_scope(&mut self) {
		self.write_scope = match self.write_scope {
			WriteScope::Global => WriteScope::Project,
			WriteScope::Project => WriteScope::Global,
		};
		self.build_rows();
		self.apply_filter();
	}

	/// The item under the cursor, if any.
	fn selected_item(&self) -> Option<&ResourceItem> {
		let position = *self.filtered.get(self.selected)?;
		match self.rows.get(position) {
			Some(Row::Item { index }) => self.items.get(*index),
			_ => None,
		}
	}

	/// Space — toggle (global) or cycle the override (project), then
	/// re-resolve so the checkboxes reflect the settings files.
	fn toggle_selected(&mut self) {
		let Some(item) = self.selected_item().cloned() else {
			return;
		};
		let result = match self.write_scope {
			WriteScope::Global => self.toggle_global(&item),
			WriteScope::Project => self.cycle_project(&item),
		};
		match result {
			Ok(()) => {
				self.status = None;
				let selected_key = self.selected_item().map(|item| item.rel.clone());
				self.reload();
				// Keep the cursor on the toggled item when it survives.
				if let Some(key) = selected_key
					&& let Some(position) =
						self.filtered
							.iter()
							.position(|&position| match &self.rows[position] {
								Row::Item { index } => self.items[*index].rel == key,
								_ => false,
							}) {
					self.selected = position;
				}
			}
			Err(error) => self.status = Some(error),
		}
	}

	/// Strip every pattern entry targeting `rel` (upstream's strip step:
	/// `+`/`-`/`!`-prefixed or plain forms), then push `+`/`-`.
	fn upsert_pattern(patterns: &[String], rel: &str, enable: bool) -> Vec<String> {
		let mut updated: Vec<String> = patterns
			.iter()
			.filter(|pattern| {
				let stripped = pattern
					.strip_prefix('+')
					.or_else(|| pattern.strip_prefix('-'))
					.or_else(|| pattern.strip_prefix('!'))
					.unwrap_or(pattern.as_str());
				stripped.trim_start_matches("./") != rel
			})
			.cloned()
			.collect();
		updated.push(format!("{}{rel}", if enable { '+' } else { '-' }));
		updated
	}

	fn strip_pattern(patterns: &[String], rel: &str) -> Vec<String> {
		patterns
			.iter()
			.filter(|pattern| {
				let stripped = pattern
					.strip_prefix('+')
					.or_else(|| pattern.strip_prefix('-'))
					.or_else(|| pattern.strip_prefix('!'))
					.unwrap_or(pattern.as_str());
				stripped.trim_start_matches("./") != rel
			})
			.cloned()
			.collect()
	}

	fn toggle_global(&mut self, item: &ResourceItem) -> Result<(), String> {
		if item.package_scope != "user" {
			return Err("project packages are edited in project mode (Tab)".to_string());
		}
		let enable = !item.effective();
		let patterns =
			self.manager
				.package_type_patterns(&item.package_source, item.resource_type, false);
		let updated = Self::upsert_pattern(&patterns, &item.rel, enable);
		self.manager.set_package_patterns(
			&item.package_source,
			item.resource_type,
			Some(&updated),
			false,
		)
	}

	/// Project cycle — upstream `getNextOverrideState`: inherit → the
	/// opposite of the inherited state → the remaining override → inherit.
	fn cycle_project(&mut self, item: &ResourceItem) -> Result<(), String> {
		let inherited_enabled = item.enabled;
		let current = item.project_override;
		let next = match current {
			None => Some(!inherited_enabled),
			Some(state) if state == !inherited_enabled => None,
			Some(_) => Some(!inherited_enabled),
		};
		let patterns =
			self.manager
				.package_type_patterns(&item.package_source, item.resource_type, true);
		let updated = match next {
			Some(enable) => Self::upsert_pattern(&patterns, &item.rel, enable),
			None => Self::strip_pattern(&patterns, &item.rel),
		};
		let updated = if updated.is_empty() {
			None
		} else {
			Some(updated)
		};
		self.manager.set_package_patterns(
			&item.package_source,
			item.resource_type,
			updated.as_deref(),
			true,
		)
	}

	pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
		use crossterm::event::{KeyCode, KeyModifiers};
		if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
			return false; // exit 0 — upstream onExit
		}
		match key.code {
			KeyCode::Esc => false,
			KeyCode::Up => {
				self.step_item(-1);
				true
			}
			KeyCode::Down => {
				self.step_item(1);
				true
			}
			KeyCode::PageUp => {
				self.page(-1);
				true
			}
			KeyCode::PageDown => {
				self.page(1);
				true
			}
			KeyCode::Tab if self.project_mode_available => {
				self.switch_scope();
				true
			}
			KeyCode::Char(' ') | KeyCode::Enter => {
				self.toggle_selected();
				true
			}
			KeyCode::Backspace => {
				self.search.pop();
				self.apply_filter();
				true
			}
			KeyCode::Char(c)
				if !key
					.modifiers
					.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
			{
				self.search.push(c);
				self.apply_filter();
				true
			}
			_ => true,
		}
	}

	fn draw(&mut self, frame: &mut Frame) {
		let area = frame.area();
		let block_title = match self.write_scope {
			WriteScope::Global => " Global Resources ",
			WriteScope::Project => " Project Local Resources ",
		};
		let [header_area, list_area, hint_area] = Layout::vertical([
			Constraint::Length(2),
			Constraint::Min(1),
			Constraint::Length(1),
		])
		.areas(area);

		// Header: title line with the mode hints right-aligned, then the
		// scope hint (upstream ConfigSelectorHeader).
		let hint = {
			let switch = if self.project_mode_available {
				"tab switch mode · "
			} else {
				""
			};
			let action = if self.write_scope == WriteScope::Project {
				"space cycle inherit/+/-"
			} else {
				"space toggle"
			};
			format!("{switch}{action} · esc close")
		};
		let title = Line::from(vec![
			Span::styled(
				block_title.trim().to_string(),
				Style::new().add_modifier(Modifier::BOLD),
			),
			Span::raw(
				" ".repeat(
					header_area
						.width
						.saturating_sub(block_title.len() as u16 + hint.len() as u16 + 2)
						.max(1) as usize,
				),
			),
			Span::styled(hint, Style::new().fg(self.muted())),
		]);
		let scope_hint = match self.write_scope {
			WriteScope::Global => format!(
				"~/…/{}/settings.json",
				self.agent_dir
					.file_name()
					.map(|name| name.to_string_lossy().to_string())
					.unwrap_or_else(|| "agent".to_string())
			),
			WriteScope::Project => {
				".pi/settings.json · inherited global resources are dimmed".to_string()
			}
		};
		frame.render_widget(
			Paragraph::new(vec![
				title,
				Line::from(Span::styled(scope_hint, Style::new().fg(self.muted()))),
			]),
			header_area,
		);

		// Search line + list inside a bordered block.
		let block = Block::default()
			.borders(Borders::ALL)
			.border_style(Style::new().fg(self.muted()));
		let inner = block.inner(list_area);
		frame.render_widget(block, list_area);
		let search_line = Line::from(vec![
			Span::styled("search: ", Style::new().fg(self.muted())),
			Span::raw(self.search.clone()),
			Span::styled("▌", Style::new().fg(self.muted())),
		]);
		let [search_area, rows_area] =
			Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
		frame.render_widget(Paragraph::new(search_line), search_area);

		if self.filtered.is_empty() {
			frame.render_widget(
				Paragraph::new(Span::styled(
					"  No resources found",
					Style::new().fg(self.muted()),
				)),
				rows_area,
			);
		} else {
			self.draw_rows(frame, rows_area);
		}

		// Bottom hint line: status errors win, else the config paths.
		let hint_text = self.status.clone().unwrap_or_else(|| {
			"pi-rs package install/remove/list/update · writes apply on next start".to_string()
		});
		let style = if self.status.is_some() {
			Style::new().fg(self.error())
		} else {
			Style::new().fg(self.muted())
		};
		frame.render_widget(Paragraph::new(Span::styled(hint_text, style)), hint_area);
	}

	/// Style shims — the selector owns no theme; ratatui indexed colors keep
	/// it legible on both light and dark terminals.
	fn muted(&self) -> ratatui::style::Color {
		ratatui::style::Color::DarkGray
	}
	fn error(&self) -> ratatui::style::Color {
		ratatui::style::Color::Red
	}

	fn draw_rows(&mut self, frame: &mut Frame, area: Rect) {
		let max_visible = self.max_visible() as usize;
		let total = self.filtered.len();
		let start = self
			.selected
			.saturating_sub(max_visible / 2)
			.min(total.saturating_sub(max_visible));
		let end = (start + max_visible).min(total);

		let project_mode = self.write_scope == WriteScope::Project;
		let mut lines = Vec::new();
		for (offset, &position) in self.filtered[start..end].iter().enumerate() {
			let is_selected = start + offset == self.selected;
			match &self.rows[position] {
				Row::Group { label, inherited } => {
					let suffix = if *inherited {
						" · inherited global"
					} else {
						""
					};
					let style = Style::new().add_modifier(Modifier::BOLD).fg(if *inherited {
						self.muted()
					} else {
						ratatui::style::Color::Reset
					});
					lines.push(Line::from(Span::styled(
						format!("  {label}{suffix}"),
						style,
					)));
				}
				Row::Subgroup { label, inherited } => {
					let style = Style::new().fg(if *inherited {
						self.muted()
					} else {
						ratatui::style::Color::Gray
					});
					lines.push(Line::from(Span::styled(format!("    {label}"), style)));
				}
				Row::Item { index } => {
					let item = &self.items[*index];
					let inherited_global = project_mode
						&& item.package_scope == "user"
						&& item.project_override.is_none();
					let (checkbox, checkbox_color) = if project_mode {
						match item.project_override {
							Some(true) => ("[+]", ratatui::style::Color::Green),
							Some(false) => ("[-]", ratatui::style::Color::Yellow),
							None => (if item.enabled { "[x]" } else { "[ ]" }, self.muted()),
						}
					} else if item.enabled {
						("[x]", ratatui::style::Color::Green)
					} else {
						("[ ]", self.muted())
					};
					let suffix = if project_mode {
						match item.project_override {
							Some(true) => "  project load",
							Some(false) => "  project unload",
							None if item.package_scope == "user" => "  inherited global",
							None => "",
						}
					} else {
						""
					};
					let name_style = if inherited_global {
						Style::new().fg(self.muted())
					} else if is_selected {
						Style::new().add_modifier(Modifier::BOLD)
					} else {
						Style::new()
					};
					let cursor = if is_selected { "❯ " } else { "  " };
					lines.push(Line::from(vec![
						Span::raw(cursor.to_string()),
						Span::styled(format!("{checkbox} "), Style::new().fg(checkbox_color)),
						Span::styled(display_name(item), name_style),
						Span::styled(suffix.to_string(), Style::new().fg(self.muted())),
					]));
				}
			}
		}
		// Scroll indicator (upstream: `(current/total)` when overflowing).
		if start > 0 || end < total {
			let count = self
				.filtered
				.iter()
				.filter(|&&position| matches!(self.rows[position], Row::Item { .. }))
				.count();
			let current = self.filtered[..=self.selected]
				.iter()
				.filter(|&&position| matches!(self.rows[position], Row::Item { .. }))
				.count();
			lines.push(Line::from(Span::styled(
				format!("  ({current}/{count})"),
				Style::new().fg(self.muted()),
			)));
		}
		frame.render_widget(Paragraph::new(lines), area);
	}
}

/// Display names like upstream `buildGroups`: extension files in subfolders
/// show `parent/file`, SKILL.md shows its directory, others the file name.
fn display_name(item: &ResourceItem) -> String {
	let file = item
		.path
		.file_name()
		.map(|name| name.to_string_lossy().to_string())
		.unwrap_or_default();
	match item.resource_type {
		ResourceType::Skills if file == "SKILL.md" => item
			.path
			.parent()
			.and_then(|parent| parent.file_name())
			.map(|name| name.to_string_lossy().to_string())
			.unwrap_or(file),
		ResourceType::Extensions => {
			let parent = item.path.parent();
			let in_extensions_root = parent
				.and_then(|parent| parent.file_name())
				.is_some_and(|name| name == "extensions");
			if in_extensions_root {
				file
			} else {
				format!(
					"{}/{}",
					parent
						.and_then(|parent| parent.file_name())
						.map(|name| name.to_string_lossy().to_string())
						.unwrap_or_default(),
					file
				)
			}
		}
		_ => file,
	}
}

fn type_label(resource_type: ResourceType) -> &'static str {
	match resource_type {
		ResourceType::Extensions => "Extensions",
		ResourceType::Skills => "Skills",
		ResourceType::Prompts => "Prompts",
		ResourceType::Themes => "Themes",
	}
}

/// Short package label: the npm name, host/path, or the directory name of a
/// local source (full sources render in the group tooltip-less label).
fn short_source(source: &str) -> String {
	match crate::core::package_manager::parse_source(source) {
		crate::core::package_manager::ParsedSource::Npm { name, .. } => name,
		crate::core::package_manager::ParsedSource::Git { host, path, .. } => {
			format!("{host}/{path}")
		}
		crate::core::package_manager::ParsedSource::Local { path } => PathBuf::from(&path)
			.file_name()
			.map(|name| name.to_string_lossy().to_string())
			.unwrap_or(path),
	}
}

/// Run the selector until Esc/ctrl+c. Returns the exit code.
pub fn run(
	manager: PackageManager,
	agent_dir: PathBuf,
	start_local: bool,
	project_mode_available: bool,
) -> i32 {
	if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
		eprintln!("pi-rs config requires a terminal");
		return 1;
	}
	let Ok(_guard) = pi_tui::TerminalGuard::new() else {
		eprintln!("failed to enter terminal raw mode");
		return 1;
	};
	let Ok(mut terminal) = ratatui::Terminal::new(CrosstermBackend::new(std::io::stdout())) else {
		eprintln!("failed to open the terminal");
		return 1;
	};
	let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
	let mut selector = ConfigSelector::new(
		manager,
		agent_dir,
		start_local,
		project_mode_available,
		rows.max(12),
	);
	loop {
		if let Ok(size) = terminal.size() {
			selector.terminal_height = size.height.max(12);
		}
		if terminal.draw(|frame| selector.draw(frame)).is_err() {
			break;
		}
		match crossterm::event::read() {
			Ok(crossterm::event::Event::Key(key)) => {
				if !selector.handle_key(key) {
					break;
				}
			}
			Ok(crossterm::event::Event::Resize(_, rows)) => {
				selector.terminal_height = rows.max(12);
			}
			_ => {}
		}
	}
	0
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn upsert_and_strip_pattern_replace_every_form_of_the_target() {
		let patterns = vec![
			"+old.md".to_string(),
			"-other.md".to_string(),
			"plain.md".to_string(),
		];
		let updated = ConfigSelector::upsert_pattern(&patterns, "old.md", false);
		assert_eq!(updated, vec!["-other.md", "plain.md", "-old.md"]);
		let updated = ConfigSelector::upsert_pattern(&updated, "old.md", true);
		assert_eq!(updated, vec!["-other.md", "plain.md", "+old.md"]);
		let stripped = ConfigSelector::strip_pattern(&updated, "old.md");
		assert_eq!(stripped, vec!["-other.md", "plain.md"]);
	}

	#[test]
	fn display_names_follow_the_upstream_rules() {
		let item = |path: &str, ty: ResourceType| ResourceItem {
			path: PathBuf::from(path),
			rel: path.to_string(),
			resource_type: ty,
			enabled: true,
			project_override: None,
			package_scope: "user",
			package_source: "pkg".to_string(),
		};
		assert_eq!(
			display_name(&item("/pkg/skills/nested/SKILL.md", ResourceType::Skills)),
			"nested"
		);
		assert_eq!(
			display_name(&item("/pkg/extensions/util.ts", ResourceType::Extensions)),
			"util.ts"
		);
		assert_eq!(
			display_name(&item(
				"/pkg/extensions/tools/index.js",
				ResourceType::Extensions
			)),
			"tools/index.js"
		);
		assert_eq!(
			display_name(&item("/pkg/prompts/one.md", ResourceType::Prompts)),
			"one.md"
		);
	}
}
