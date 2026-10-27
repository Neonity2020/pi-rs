//! Markdown component — counterpart of
//! `pi/packages/tui/src/components/markdown.ts`.
//!
//! Parses with pulldown-cmark and renders to themed ratatui lines with the
//! same visual vocabulary as pi: h1 bold+underline, h3+ keeps its `#` prefix,
//! fenced code blocks keep their ``` fences with a 2-space indent, quotes get
//! a `│ ` border and italic text, lists use 4-space indent per nesting level
//! with aligned continuation lines, links underline and show the URL when it
//! differs from the label. Word-wrapping is greedy on unicode width.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::Component;
use crate::theme::Theme;

/// Render a markdown source string to themed lines at the given width.
pub fn markdown_lines(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
	Renderer::new(width, theme).render(source)
}

/// One styled run inside a block being accumulated.
struct Run {
	text: String,
	style: Style,
}

pub struct Renderer<'t> {
	width: usize,
	theme: &'t Theme,
	lines: Vec<Line<'static>>,
	/// Inline runs of the current block (paragraph/heading/table cell/list item).
	runs: Vec<Run>,
	/// Style stack from Strong/Em/Strike/Link/heading/quote nesting.
	style_stack: Vec<Style>,
	/// Open link destinations (parallel to Link entries in style_stack).
	link_stack: Vec<String>,
	/// Nesting depth of block quotes; every emitted line gets `│ ` borders.
	quote_depth: usize,
	/// Stack of open lists (indent, ordered, counter, loose).
	lists: Vec<ListFrame>,
	/// Marker spans for the current list item (first-line prefix).
	item_prefix: Vec<Span<'static>>,
	/// Width of the current item's first-line prefix (for wrapping).
	item_prefix_width: usize,
	/// Set once the first line of the current list item was flushed.
	item_started: bool,
	/// Current fenced code block: language + raw lines.
	code_block: Option<(String, Vec<String>)>,
	/// Blank line deferred until the next block actually emits content
	/// (drops the blank between a paragraph and a following list, like pi).
	pending_blank: bool,
	/// Table state: column aligns + rows of cell runs.
	table: Option<TableState>,
}

struct ListFrame {
	/// Spaces accumulated from enclosing list indents.
	indent: String,
	ordered: bool,
	counter: u64,
	loose: bool,
}

#[derive(Default)]
struct TableState {
	aligns: Vec<pulldown_cmark::Alignment>,
	header: Vec<Vec<Span<'static>>>,
	rows: Vec<Vec<Vec<Span<'static>>>>,
	current_row: Vec<Vec<Span<'static>>>,
}

impl<'t> Renderer<'t> {
	fn new(width: usize, theme: &'t Theme) -> Self {
		Self {
			width: width.max(1),
			theme,
			lines: Vec::new(),
			runs: Vec::new(),
			style_stack: Vec::new(),
			link_stack: Vec::new(),
			quote_depth: 0,
			lists: Vec::new(),
			item_prefix: Vec::new(),
			item_prefix_width: 0,
			item_started: false,
			code_block: None,
			pending_blank: false,
			table: None,
		}
	}

	// -- style resolution -----------------------------------------------------

	/// Base text style: current stack (strong/em/link…) over quote/heading base.
	fn current_style(&self) -> Style {
		let mut style = Style::default();
		if self.quote_depth > 0 {
			style = style.fg(self.theme.md_quote).add_modifier(Modifier::ITALIC);
		}
		for s in &self.style_stack {
			style = style.patch(*s);
		}
		style
	}

	fn push_run(&mut self, text: &str) {
		if text.is_empty() {
			return;
		}
		let style = self.current_style();
		if let Some(last) = self.runs.last_mut().filter(|last| last.style == style) {
			last.text.push_str(text);
			return;
		}
		self.runs.push(Run {
			text: text.to_string(),
			style,
		});
	}

	// -- block emission helpers -----------------------------------------------

	/// Content width after quote borders and list indents.
	fn effective_width(&self) -> usize {
		let mut w = self.width.saturating_sub(self.quote_depth * 2);
		for frame in &self.lists {
			w = w.saturating_sub(frame.indent.width() + 4);
		}
		w.max(1)
	}

	fn quote_borders(&self) -> Vec<Span<'static>> {
		vec![Span::styled("│ ", Style::new().fg(self.theme.md_quote_border)); self.quote_depth]
	}

	/// Flush deferred blank; call before a block emits its first line.
	fn open_block(&mut self) {
		if self.pending_blank && !self.lines.is_empty() {
			self.lines.push(Line::default());
		}
		self.pending_blank = false;
	}

	/// Queue a blank separator (realized only if another block follows).
	fn close_block(&mut self) {
		self.pending_blank = true;
	}

	/// Push one raw content line (already wrap-free), applying quote borders.
	fn push_raw(&mut self, spans: Vec<Span<'static>>) {
		self.open_block();
		let mut line = self.quote_borders();
		line.extend(spans);
		self.lines.push(Line::from(line));
	}

	/// Wrap the accumulated inline runs and emit them under the given prefixes.
	fn flush_inline(&mut self, mut first_prefix: Vec<Span<'static>>, cont_prefix: String) {
		if self.runs.is_empty() && first_prefix.is_empty() {
			return;
		}
		let first_w = first_prefix
			.iter()
			.map(|s| s.content.width())
			.sum::<usize>();
		let content_width = self.effective_width().saturating_sub(first_w).max(1);
		let wrapped = wrap_runs(&self.runs, content_width);
		self.runs.clear();

		if wrapped.is_empty() {
			if first_prefix.is_empty() {
				return; // empty paragraph flush — nothing to emit
			}
			// Bare marker line (empty list item).
			let mut line = self.quote_borders();
			line.append(&mut first_prefix);
			self.open_block();
			self.lines.push(Line::from(line));
			return;
		}

		let cont_style = self.current_style();
		// First-line prefix vs continuation only applies inside list items;
		// elsewhere the prefix is a one-shot (heading `#`) or empty.
		let used_prefix = self.item_started && !self.lists.is_empty();
		let mut first_prefix = first_prefix;
		for (i, wline) in wrapped.into_iter().enumerate() {
			let mut line = self.quote_borders();
			if i == 0 && !used_prefix {
				line.append(&mut first_prefix);
			} else {
				line.push(Span::styled(cont_prefix.clone(), cont_style));
			}
			line.extend(wline);
			self.open_block();
			self.lines.push(Line::from(line));
		}
		self.item_started = true;
	}

	/// Flush inline with the current prefixes (list-item marker or heading `#`).
	fn flush_block_inline(&mut self) {
		let in_list = !self.lists.is_empty();
		let prefix = std::mem::take(&mut self.item_prefix);
		let cont = if in_list {
			" ".repeat(self.item_prefix_width)
		} else {
			String::new()
		};
		self.flush_inline(prefix, cont);
	}

	// -- main entry -------------------------------------------------------------

	fn render(mut self, source: &str) -> Vec<Line<'static>> {
		let mut options = Options::empty();
		options.insert(Options::ENABLE_STRIKETHROUGH);
		options.insert(Options::ENABLE_TASKLISTS);
		options.insert(Options::ENABLE_TABLES);

		for event in Parser::new_ext(source, options) {
			match event {
				Event::Start(tag) => self.start_tag(tag),
				Event::End(tag) => self.end_tag(tag),
				Event::Text(text) => self.inline_text(&text),
				Event::Code(code) => {
					let style = self.current_style().fg(self.theme.md_code);
					self.runs.push(Run {
						text: code.to_string(),
						style,
					});
				}
				Event::InlineHtml(html) => self.push_run(&html),
				Event::Html(html) => {
					if self.code_block.is_none() && self.table.is_none() {
						self.push_raw(vec![Span::raw(html.trim_end().to_string())]);
					}
				}
				Event::SoftBreak => self.push_run(" "),
				Event::HardBreak => {
					let prefix = std::mem::take(&mut self.item_prefix);
					let cont = " ".repeat(self.item_prefix_width);
					self.flush_inline(prefix, cont);
				}
				Event::Rule => {
					self.flush_block_inline();
					let rule = "─".repeat(self.effective_width().min(80));
					self.push_raw(vec![Span::styled(rule, Style::new().fg(self.theme.md_hr))]);
					self.close_block();
				}
				Event::TaskListMarker(checked) => {
					let marker = if checked { "[x] " } else { "[ ] " };
					self.item_prefix.push(Span::styled(
						marker.to_string(),
						Style::new().fg(self.theme.md_list_bullet),
					));
					self.item_prefix_width += 4;
				}
				Event::FootnoteReference(_) => {}
				// Math (ENABLE_MATH not set; plain text fallback) and other events.
				Event::InlineMath(text) | Event::DisplayMath(text) => self.push_run(&text),
			}
		}
		// Trailing inline (unterminated paragraph) or deferred blank.
		if !self.runs.is_empty() {
			self.flush_block_inline();
		}
		while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
			self.lines.pop();
		}
		self.lines
	}

	// -- tags ---------------------------------------------------------------------

	fn start_tag(&mut self, tag: Tag) {
		match tag {
			Tag::Paragraph => self.runs.clear(),
			Tag::Heading { level, .. } => {
				self.flush_block_inline();
				self.runs.clear();
				let mut style = Style::new()
					.fg(self.theme.md_heading)
					.add_modifier(Modifier::BOLD);
				if level == HeadingLevel::H1 {
					style = style.add_modifier(Modifier::UNDERLINED);
				}
				self.style_stack.push(style);
				if level >= HeadingLevel::H3 {
					let n = heading_level_number(level);
					let prefix = format!("{} ", "#".repeat(n));
					self.item_prefix_width = prefix.width();
					self.item_prefix.push(Span::styled(prefix, style));
				}
			}
			Tag::BlockQuote(_) => {
				self.flush_block_inline();
				self.quote_depth += 1;
			}
			Tag::CodeBlock(kind) => {
				self.flush_block_inline();
				let lang = match kind {
					CodeBlockKind::Fenced(info) => {
						info.split_whitespace().next().unwrap_or("").to_string()
					}
					CodeBlockKind::Indented => String::new(),
				};
				self.code_block = Some((lang, Vec::new()));
			}
			Tag::List(start) => {
				self.flush_block_inline();
				let indent = self
					.lists
					.last()
					.map(|f| format!("{}    ", f.indent))
					.unwrap_or_default();
				self.lists.push(ListFrame {
					indent,
					ordered: start.is_some(),
					counter: start.unwrap_or(1),
					loose: false,
				});
				self.pending_blank = false; // no blank between paragraph and list (pi)
			}
			Tag::Item => {
				let frame = self.lists.last_mut().expect("item outside list");
				let marker = if frame.ordered {
					format!("{}. ", frame.counter)
				} else {
					"- ".to_string()
				};
				frame.counter += 1;
				self.item_started = false;
				self.item_prefix_width = marker.width();
				self.item_prefix = vec![Span::styled(
					marker,
					Style::new().fg(self.theme.md_list_bullet),
				)];
			}
			Tag::Emphasis => self
				.style_stack
				.push(Style::new().add_modifier(Modifier::ITALIC)),
			Tag::Strong => self
				.style_stack
				.push(Style::new().add_modifier(Modifier::BOLD)),
			Tag::Strikethrough => self
				.style_stack
				.push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
			Tag::Link { dest_url, .. } => {
				let style = Style::new()
					.fg(self.theme.md_link)
					.add_modifier(Modifier::UNDERLINED);
				self.style_stack.push(style);
				self.link_stack.push(dest_url.to_string());
			}
			Tag::Image { .. } => self.style_stack.push(Style::new().fg(self.theme.md_link)),
			Tag::Table(aligns) => {
				self.flush_block_inline();
				self.table = Some(TableState {
					aligns: aligns.into_iter().collect(),
					..TableState::default()
				});
			}
			Tag::TableHead | Tag::TableRow => {
				if let Some(t) = self.table.as_mut() {
					t.current_row.clear();
				}
			}
			Tag::TableCell => self.runs.clear(),
			Tag::FootnoteDefinition(_) => {}
			Tag::HtmlBlock => {}
			Tag::MetadataBlock(_) => {}
			Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {}
			Tag::Superscript | Tag::Subscript => {}
		}
	}

	fn end_tag(&mut self, tag: TagEnd) {
		match tag {
			TagEnd::Paragraph => {
				self.flush_block_inline();
				self.close_block();
			}
			TagEnd::Heading(level) => {
				// H3+ keep their `#` prefix; move it in front of the text runs.
				if level >= HeadingLevel::H3 {
					let prefix = std::mem::take(&mut self.item_prefix);
					self.runs.splice(
						0..0,
						prefix.into_iter().map(|s| Run {
							text: s.content.to_string(),
							style: s.style,
						}),
					);
					self.item_prefix_width = 0;
				}
				self.style_stack.pop();
				self.flush_block_inline();
				self.close_block();
			}
			TagEnd::BlockQuote(_) => {
				self.flush_block_inline();
				self.quote_depth = self.quote_depth.saturating_sub(1);
				self.close_block();
			}
			TagEnd::CodeBlock => {
				if let Some((lang, code)) = self.code_block.take() {
					let border = Style::new().fg(self.theme.md_code_block_border);
					let body = Style::new().fg(self.theme.md_code_block);
					let indent = "  ";
					let head = format!("```{lang}");
					self.push_raw(vec![Span::styled(head, border)]);
					self.open_block();
					let content_width = self.effective_width().saturating_sub(2).max(1);
					for src in code {
						for (i, chunk) in hard_wrap(&src, content_width).into_iter().enumerate() {
							let mut line = self.quote_borders();
							if i == 0 {
								line.push(Span::raw(indent.to_string()));
							} else {
								line.push(Span::raw("    ".to_string()));
							}
							line.push(Span::styled(chunk, body));
							self.lines.push(Line::from(line));
						}
					}
					self.lines.last_mut().map(strip_trailing_space);
					self.push_raw(vec![Span::styled("```", border)]);
					self.close_block();
				}
			}
			TagEnd::List(_) => {
				self.flush_block_inline();
				if self.lists.pop().is_some_and(|frame| frame.loose) {
					self.close_block();
				}
				if self.lists.is_empty() {
					self.close_block();
				}
			}
			TagEnd::Item => {
				self.flush_block_inline();
			}
			TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
				self.style_stack.pop();
			}
			TagEnd::Link | TagEnd::Image => {
				self.style_stack.pop();
				// When a labeled link closes, show the URL after the text.
				if let Some(url) = self.link_stack.pop() {
					let text: String = self
						.runs
						.iter()
						.map(|r| r.text.as_str())
						.collect::<Vec<_>>()
						.concat();
					if !text.is_empty() && text != url {
						let url_style = Style::new().fg(self.theme.md_link_url);
						self.runs.push(Run {
							text: format!(" ({url})"),
							style: url_style,
						});
					}
				}
			}
			TagEnd::Table => {
				self.flush_block_inline();
				if let Some(t) = self.table.take() {
					self.render_table(t);
					self.close_block();
				}
			}
			TagEnd::TableHead => {
				if let Some(t) = self.table.as_mut() {
					t.header = std::mem::take(&mut t.current_row);
				}
			}
			TagEnd::TableRow => {
				if let Some(t) = self.table.as_mut() {
					t.rows.push(std::mem::take(&mut t.current_row));
				}
			}
			TagEnd::TableCell => {
				let cell = std::mem::take(&mut self.runs);
				let spans = flatten_runs(&cell);
				if let Some(t) = self.table.as_mut() {
					t.current_row.push(spans);
				}
			}
			TagEnd::HtmlBlock | TagEnd::MetadataBlock(_) => {}
			TagEnd::FootnoteDefinition => {}
			TagEnd::DefinitionList
			| TagEnd::DefinitionListTitle
			| TagEnd::DefinitionListDefinition
			| TagEnd::Superscript
			| TagEnd::Subscript => {}
		}
	}

	fn inline_text(&mut self, text: &str) {
		if let Some((_, code)) = self.code_block.as_mut() {
			for line in text.split('\n') {
				// pulldown ends fenced blocks with a trailing newline.
				if text.ends_with('\n') && line.is_empty() {
					continue;
				}
				code.push(line.to_string());
			}
			return;
		}
		// Paragraph text: soft breaks already arrived as SoftBreak events, so
		// interior newlines only occur in tight contexts (list items) — keep them.
		self.push_run(text);
	}

	// -- tables ---------------------------------------------------------------------

	fn render_table(&mut self, t: TableState) {
		if t.header.is_empty() && t.rows.is_empty() {
			return;
		}
		let n_cols = t
			.header
			.len()
			.max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
		if n_cols == 0 {
			return;
		}
		// Column widths from cell content (single-line projection).
		let mut widths = vec![0usize; n_cols];
		let cell_text = |cell: &Vec<Span<'static>>| -> String {
			cell.iter()
				.map(|s| s.content.as_ref())
				.collect::<Vec<_>>()
				.join("")
		};
		for (i, cell) in t.header.iter().enumerate() {
			widths[i] = widths[i].max(cell_text(cell).width());
		}
		for row in &t.rows {
			for (i, cell) in row.iter().enumerate() {
				widths[i] = widths[i].max(cell_text(cell).width());
			}
		}
		for w in widths.iter_mut() {
			*w = (*w).clamp(3, 40);
		}

		let border = Style::new().fg(self.theme.md_hr);
		let render_row = |row: &[Vec<Span<'static>>], bold: bool| -> Vec<Span<'static>> {
			let mut spans = vec![Span::styled("│", border)];
			for (i, w) in widths.iter().enumerate() {
				let cell = row.get(i);
				let text = cell.map(cell_text).unwrap_or_default();
				let style = if bold {
					Style::new().add_modifier(Modifier::BOLD)
				} else {
					Style::default()
				};
				let padded = match t.aligns.get(i) {
					Some(pulldown_cmark::Alignment::Right) => format!("{text:>w$}"),
					Some(pulldown_cmark::Alignment::Center) => {
						let total = w.saturating_sub(text.width());
						let left = total / 2;
						format!("{}{text}{}", " ".repeat(left), " ".repeat(total - left))
					}
					_ => format!("{text:<w$}"),
				};
				spans.push(Span::raw(" "));
				spans.push(Span::styled(padded, style));
				spans.push(Span::raw(" "));
				spans.push(Span::styled("│", border));
			}
			spans
		};

		self.push_raw(render_row(&t.header, true));
		let sep: Vec<Span<'static>> = {
			let mut spans = vec![Span::styled("│", border)];
			for w in &widths {
				spans.push(Span::styled("─".repeat(w + 2), border));
				spans.push(Span::styled("│", border));
			}
			spans
		};
		self.push_raw(sep);
		for row in &t.rows {
			self.push_raw(render_row(row, false));
		}
	}
}

fn heading_level_number(level: HeadingLevel) -> usize {
	match level {
		HeadingLevel::H1 => 1,
		HeadingLevel::H2 => 2,
		HeadingLevel::H3 => 3,
		HeadingLevel::H4 => 4,
		HeadingLevel::H5 => 5,
		HeadingLevel::H6 => 6,
	}
}

// -- wrapping -------------------------------------------------------------------

/// Split runs into words and greedily wrap at `width`. Interior '\n' in a run
/// forces a break. Very long words are hard-broken.
fn wrap_runs(runs: &[Run], width: usize) -> Vec<Vec<Span<'static>>> {
	if runs.iter().all(|r| r.text.is_empty()) {
		return Vec::new();
	}
	// Flatten to words with their styles, keeping explicit breaks.
	enum Piece {
		Word(String, Style),
		Space(Style), // collapsible whitespace between words
		Break,
	}
	let mut pieces: Vec<Piece> = Vec::new();
	for run in runs {
		let mut segment = String::new();
		let mut segment_style = run.style;
		let flush = |segment: &mut String, pieces: &mut Vec<Piece>, style: Style| {
			if !segment.is_empty() {
				pieces.push(Piece::Word(std::mem::take(segment), style));
			}
		};
		for ch in run.text.chars() {
			match ch {
				'\n' => {
					flush(&mut segment, &mut pieces, segment_style);
					pieces.push(Piece::Break);
					segment_style = run.style;
				}
				' ' | '\t' => {
					flush(&mut segment, &mut pieces, segment_style);
					// Collapse whitespace runs.
					if !matches!(
						pieces.last(),
						Some(Piece::Space(_)) | Some(Piece::Break) | None
					) {
						pieces.push(Piece::Space(run.style));
					}
					segment_style = run.style;
				}
				c => {
					if segment_style != run.style && !segment.is_empty() {
						flush(&mut segment, &mut pieces, segment_style);
						segment_style = run.style;
					}
					segment.push(c);
				}
			}
		}
		flush(&mut segment, &mut pieces, segment_style);
	}

	let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
	let mut current: Vec<Span<'static>> = Vec::new();
	let mut current_w = 0usize;
	let mut started = false;
	// Whitespace held back until the next word: dropped at line breaks so
	// wrapped lines never carry trailing spaces.
	let mut pending_space: Option<Style> = None;

	let push_span = |current: &mut Vec<Span<'static>>, text: String, style: Style| {
		if let Some(last) = current.last_mut().filter(|last| last.style == style) {
			last.content.to_mut().push_str(&text);
			return;
		}
		current.push(Span::styled(text, style));
	};

	for piece in pieces {
		match piece {
			Piece::Break => {
				lines.push(std::mem::take(&mut current));
				current_w = 0;
				started = false;
				pending_space = None;
			}
			Piece::Space(style) => {
				if started {
					pending_space = Some(style);
				}
			}
			Piece::Word(word, style) => {
				let w = word.width();
				if w > width {
					// Hard-break oversized words.
					if started {
						lines.push(std::mem::take(&mut current));
						current_w = 0;
					}
					pending_space = None;
					let mut chunk = String::new();
					let mut chunk_w = 0usize;
					for c in word.chars() {
						let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
						if chunk_w + cw > width && !chunk.is_empty() {
							push_span(&mut current, std::mem::take(&mut chunk), style);
							lines.push(std::mem::take(&mut current));
							chunk_w = 0;
						}
						chunk.push(c);
						chunk_w += cw;
					}
					if !chunk.is_empty() {
						push_span(&mut current, chunk, style);
						current_w = chunk_w;
					}
					started = true;
				} else {
					let space_style = pending_space.take();
					let extra = if started && space_style.is_some() {
						1
					} else {
						0
					};
					if started && current_w + extra + w > width {
						lines.push(std::mem::take(&mut current));
						current_w = 0;
					} else if started && let Some(style) = space_style {
						push_span(&mut current, " ".to_string(), style);
						current_w += 1;
					}
					push_span(&mut current, word, style);
					current_w += w;
					started = true;
				}
			}
		}
	}
	if !current.is_empty() {
		lines.push(current);
	}
	lines
}

/// Hard-wrap a raw line at `width` (for code blocks), respecting unicode width.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
	if line.width() <= width {
		return vec![line.to_string()];
	}
	let mut out = Vec::new();
	let mut chunk = String::new();
	let mut w = 0usize;
	for c in line.chars() {
		let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
		if w + cw > width {
			out.push(std::mem::take(&mut chunk));
			w = 0;
		}
		chunk.push(c);
		w += cw;
	}
	if !chunk.is_empty() {
		out.push(chunk);
	}
	out
}

fn flatten_runs(runs: &[Run]) -> Vec<Span<'static>> {
	let mut spans: Vec<Span<'static>> = Vec::new();
	for run in runs {
		if run.text.is_empty() {
			continue;
		}
		if let Some(last) = spans.last_mut().filter(|last| last.style == run.style) {
			last.content.to_mut().push_str(&run.text);
			continue;
		}
		spans.push(Span::styled(run.text.clone(), run.style));
	}
	spans
}

fn strip_trailing_space(line: &mut Line<'static>) {
	if let Some(last) = line.spans.last_mut() {
		let trimmed = last.content.trim_end().to_string();
		last.content = trimmed.into();
	}
}

/// Markdown component (retained source; renders on demand).
pub struct Markdown {
	source: String,
	theme: Theme,
}

impl Markdown {
	pub fn new(source: impl Into<String>, theme: Theme) -> Self {
		Self {
			source: source.into(),
			theme,
		}
	}

	pub fn set_source(&mut self, source: impl Into<String>) {
		self.source = source.into();
	}

	pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
		markdown_lines(&self.source, width, &self.theme)
	}
}

impl Component for Markdown {
	fn handle_event(&mut self, _event: &crossterm::event::Event) -> bool {
		false
	}

	fn render(&mut self, area: Rect, buf: &mut Buffer) {
		let lines = self.lines(area.width as usize);
		Paragraph::new(ratatui::text::Text::from(lines)).render(area, buf);
	}

	fn preferred_height(&self, width: u16) -> u16 {
		self.lines(width as usize).len() as u16
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn plain(lines: Vec<Line<'static>>) -> Vec<String> {
		lines
			.into_iter()
			.map(|l| {
				l.spans
					.iter()
					.map(|s| s.content.as_ref())
					.collect::<String>()
			})
			.collect()
	}

	#[test]
	fn headings_and_paragraphs() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("# Title\n\nbody text\n", 40, &theme));
		assert_eq!(out, vec!["Title", "", "body text"]);
	}

	#[test]
	fn h3_keeps_hash_prefix() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("### Sub\n", 40, &theme));
		assert_eq!(out, vec!["### Sub"]);
	}

	#[test]
	fn wraps_long_paragraphs() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("aaaa bbbb cccc dddd\n", 10, &theme));
		assert_eq!(out, vec!["aaaa bbbb", "cccc dddd"]);
	}

	#[test]
	fn code_block_fences_and_indent() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("```rust\nlet x = 1;\n```\n", 40, &theme));
		assert_eq!(out, vec!["```rust", "  let x = 1;", "```"]);
	}

	#[test]
	fn list_markers_and_continuation() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("- first\n- second item here\n", 40, &theme));
		assert_eq!(out, vec!["- first", "- second item here"]);
	}

	#[test]
	fn ordered_list_numbers() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("3. three\n4. four\n", 40, &theme));
		assert_eq!(out, vec!["3. three", "4. four"]);
	}

	#[test]
	fn blockquote_border() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("> quoted text\n", 40, &theme));
		assert_eq!(out, vec!["│ quoted text"]);
	}

	#[test]
	fn inline_code_and_hard_word_wrap() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("use `x` here\n", 40, &theme));
		assert_eq!(out, vec!["use x here"]);
		let out = plain(markdown_lines("abcdefghijkl\n", 5, &theme));
		assert_eq!(out, vec!["abcde", "fghij", "kl"]);
	}

	#[test]
	fn hr_and_table() {
		let theme = Theme::dark();
		let out = plain(markdown_lines("---\n", 20, &theme));
		assert_eq!(out, vec!["────────────────────"]);
		let out = plain(markdown_lines(
			"| a | b |\n|---|---|\n| 1 | 2 |\n",
			40,
			&theme,
		));
		assert_eq!(out.iter().filter(|l| l.contains("│")).count(), 3);
	}
}
