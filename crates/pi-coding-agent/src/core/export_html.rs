//! Session HTML export — port of `pi/packages/coding-agent/src/core/export-html/`
//! (`index.ts` + `template.js` render pipeline + `ansi-to-html.ts`, with
//! `tool-renderer.ts`'s ANSI→HTML pipeline folded in) and the output-path
//! rules of `exportToHtml` (`core/agent-session.ts:4348`) /
//! `exportSessionToHtml` (`export-html/index.ts:238`).
//!
//! Upstream ships a client-side rendered single-file app: the session data is
//! embedded as base64 JSON and `template.js` renders it in the browser with
//! marked.js (markdown), highlight.js (code) and toggle handlers. pi-rs has
//! no vendored JS bundles to embed, so this module renders the same DOM
//! server-side and deviates as follows (each mirrors a task-level decision):
//!
//! - Markdown is NOT converted; message text is HTML-escaped plain text
//!   (upstream: `marked.parse` via `safeMarkedParse`).
//! - Syntax highlighting is not applied; highlighted `<pre><code class="hljs">`
//!   blocks keep the structure but carry escaped plain text (upstream: hljs).
//! - Tool outputs are run through the ported ANSI→HTML converter
//!   (`ansi_to_html`); upstream reaches ANSI handling only through
//!   `tool-renderer.ts`'s TUI→ANSI→HTML pre-render pipeline for custom tools
//!   (which needs extension renderers pi-rs does not have) and shows raw
//!   escape characters for builtin tool output. For text without escapes the
//!   converter output is identical to `escapeHtml`.
//! - Interactive chrome is omitted: sidebar tree, search, filters, copy/share
//!   buttons, JSONL download and the toggle keyboard handlers have no
//!   server-side equivalent. Expandable blocks keep the upstream DOM shape
//!   (`output-preview`/`output-full`/`expand-hint`, click-to-expand classes)
//!   plus the CSS that toggles them.
//! - Theme: pi-rs has no theme system; the upstream default dark theme's
//!   resolved values are hardcoded below. The hex colors were produced by
//!   running the upstream `colorToHex` pipeline
//!   (`modes/interactive/theme/` + `packages/tui/src/oklab.ts`) over
//!   `dark.json` — e.g. `text` = `okhsl(234 3% 89%)` → `#dee0e1`.
//! - Timestamps render as the UTC `HH:MM:SS` slice of the ISO string
//!   (upstream: `toLocaleTimeString` in the viewer's locale/timezone).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::APP_NAME;
use crate::core::session_manager::{
	EntryKind, SessionEntry, SessionHeader, build_session_path, load_session_file,
};

// ---------------------------------------------------------------------------
// ANSI escape → HTML (full port of `export-html/ansi-to-html.ts`)
// ---------------------------------------------------------------------------

/// Standard ANSI color palette (0-15), verbatim.
const ANSI_COLORS: [&str; 16] = [
	"#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
	"#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

/// `color256ToHex`: 0-15 palette, 16-231 6×6×6 cube, 232-255 grayscale.
fn color_256_to_hex(index: u32) -> String {
	if index < 16 {
		return ANSI_COLORS[index as usize].to_string();
	}
	if index < 232 {
		let cube = index - 16;
		let component = |n: u32| {
			if n == 0 { 0 } else { 55 + n * 40 }
		};
		let to_hex = |n: u32| format!("{n:02x}");
		return format!(
			"#{}{}{}",
			to_hex(component(cube / 36)),
			to_hex(component((cube % 36) / 6)),
			to_hex(component(cube % 6)),
		);
	}
	let gray = 8 + (index - 232) * 10;
	format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// HTML escaping for ANSI text — `escapeHtml` in ansi-to-html.ts (note the
/// `&#039;` form; template.js's own escaper uses `&#39;`).
fn escape_ansi_text(text: &str) -> String {
	text.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
		.replace('\'', "&#039;")
}

#[derive(Default, Clone)]
struct TextStyle {
	fg: Option<String>,
	bg: Option<String>,
	bold: bool,
	dim: bool,
	italic: bool,
	underline: bool,
}

impl TextStyle {
	fn has_style(&self) -> bool {
		self.fg.is_some()
			|| self.bg.is_some()
			|| self.bold
			|| self.dim
			|| self.italic
			|| self.underline
	}

	/// `styleToInlineCSS`.
	fn to_inline_css(&self) -> String {
		let mut parts: Vec<String> = Vec::new();
		if let Some(fg) = &self.fg {
			parts.push(format!("color:{fg}"));
		}
		if let Some(bg) = &self.bg {
			parts.push(format!("background-color:{bg}"));
		}
		if self.bold {
			parts.push("font-weight:bold".into());
		}
		if self.dim {
			parts.push("opacity:0.6".into());
		}
		if self.italic {
			parts.push("font-style:italic".into());
		}
		if self.underline {
			parts.push("text-decoration:underline".into());
		}
		parts.join(";")
	}
}

/// `applySgrCode`: SGR parameter list → style mutation. Unrecognized codes are
/// ignored.
fn apply_sgr_code(params: &[u32], style: &mut TextStyle) {
	let mut i = 0;
	while i < params.len() {
		let code = params[i];
		match code {
			0 => *style = TextStyle::default(),
			1 => style.bold = true,
			2 => style.dim = true,
			3 => style.italic = true,
			4 => style.underline = true,
			22 => {
				style.bold = false;
				style.dim = false;
			}
			23 => style.italic = false,
			24 => style.underline = false,
			30..=37 => style.fg = Some(ANSI_COLORS[(code - 30) as usize].to_string()),
			38 => {
				// Extended foreground: 38;5;N (256-color) or 38;2;R;G;B (RGB).
				if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
					style.fg = Some(color_256_to_hex(params[i + 2]));
					i += 2;
				} else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
					style.fg = Some(format!(
						"rgb({},{},{})",
						params[i + 2],
						params[i + 3],
						params[i + 4]
					));
					i += 4;
				}
			}
			39 => style.fg = None,
			40..=47 => style.bg = Some(ANSI_COLORS[(code - 40) as usize].to_string()),
			48 => {
				if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
					style.bg = Some(color_256_to_hex(params[i + 2]));
					i += 2;
				} else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
					style.bg = Some(format!(
						"rgb({},{},{})",
						params[i + 2],
						params[i + 3],
						params[i + 4]
					));
					i += 4;
				}
			}
			49 => style.bg = None,
			90..=97 => style.fg = Some(ANSI_COLORS[(code - 90 + 8) as usize].to_string()),
			100..=107 => style.bg = Some(ANSI_COLORS[(code - 100 + 8) as usize].to_string()),
			_ => {}
		}
		i += 1;
	}
}

/// `ansiToHtml`: convert SGR escape sequences (`ESC[...m`) to inline-styled
/// spans; all other text is HTML-escaped.
pub fn ansi_to_html(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut style = TextStyle::default();
	let mut result = String::new();
	let mut last_index = 0usize;
	let mut in_span = false;
	let mut i = 0usize;

	while i < bytes.len() {
		// Scan for ESC '[' ... 'm' with only digits and ';' between.
		if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
			let mut end = i + 2;
			while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b';') {
				end += 1;
			}
			if end < bytes.len() && bytes[end] == b'm' {
				let before = &text[last_index..i];
				if !before.is_empty() {
					result.push_str(&escape_ansi_text(before));
				}
				if in_span {
					result.push_str("</span>");
					in_span = false;
				}
				let param_str = &text[i + 2..end];
				let params: Vec<u32> = if param_str.is_empty() {
					vec![0]
				} else {
					param_str
						.split(';')
						.map(|p| p.parse::<u32>().unwrap_or(0))
						.collect()
				};
				apply_sgr_code(&params, &mut style);
				if style.has_style() {
					result.push_str(&format!("<span style=\"{}\">", style.to_inline_css()));
					in_span = true;
				}
				i = end + 1;
				last_index = i;
				continue;
			}
		}
		i += 1;
	}

	let remaining = &text[last_index..];
	if !remaining.is_empty() {
		result.push_str(&escape_ansi_text(remaining));
	}
	if in_span {
		result.push_str("</span>");
	}
	result
}

/// `ansiLinesToHtml`: each line wrapped in `<div class="ansi-line">`; blank
/// rendered lines become `&nbsp;`.
fn ansi_lines_to_html(lines: &[&str]) -> String {
	lines
		.iter()
		.map(|line| {
			let html = ansi_to_html(line);
			let content = if html.is_empty() { "&nbsp;" } else { &html };
			format!("<div class=\"ansi-line\">{content}</div>")
		})
		.collect()
}

// ---------------------------------------------------------------------------
// Small helpers (template.js ports)
// ---------------------------------------------------------------------------

/// `escapeHtml` (template.js flavor: `&#39;`).
fn escape_html(text: &str) -> String {
	text.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
		.replace('\'', "&#39;")
}

/// `replaceTabs`: tab → three spaces.
fn replace_tabs(text: &str) -> String {
	text.replace('\t', "   ")
}

/// `shortenPath`: `/Users/<name>/…` and `/home/<name>/…` → `~/…`.
fn shorten_path(path: &str) -> String {
	for prefix in ["/Users/", "/home/"] {
		if let Some(rest) = path.strip_prefix(prefix)
			&& let Some(slash) = rest.find('/')
		{
			return format!("~/{}", &rest[slash + 1..]);
		}
	}
	path.to_string()
}

/// `formatTimestamp`: the viewer-locale time string upstream; the port slices
/// the UTC `HH:MM:SS` out of the ISO timestamp (`…T{HH:MM:SS}.{mmm}Z`).
fn format_timestamp(ts: Option<&str>) -> String {
	let Some(ts) = ts else {
		return String::new();
	};
	let bytes = ts.as_bytes();
	if bytes.len() >= 19 && bytes[10] == b'T' {
		ts[11..19].to_string()
	} else {
		String::new()
	}
}

/// `formatTokens`: raw / `N.Nk` / `Nk` / `N.NM`.
fn format_tokens(count: u64) -> String {
	if count < 1000 {
		count.to_string()
	} else if count < 10_000 {
		format!("{:.1}k", count as f64 / 1000.0)
	} else if count < 1_000_000 {
		format!("{}k", count / 1000)
	} else {
		format!("{:.1}M", count as f64 / 1_000_000.0)
	}
}

/// `str(value)` for tool arguments: `Some("")` for missing/null (JS `== null`),
/// `Some(text)` for strings, `None` for other types (renders `[invalid arg]`).
fn arg_str(args: &serde_json::Value, key: &str) -> Option<String> {
	match args.get(key) {
		None | Some(serde_json::Value::Null) => Some(String::new()),
		Some(serde_json::Value::String(text)) => Some(text.clone()),
		Some(_) => None,
	}
}

/// `getLanguageFromPath` (extension → hljs language name; highlighting itself
/// is not applied, see module docs).
fn language_from_path(path: &str) -> Option<&'static str> {
	let ext = path.rsplit('.').next()?.to_lowercase();
	let mapped = match ext.as_str() {
		"ts" | "tsx" => "typescript",
		"js" | "jsx" => "javascript",
		"py" => "python",
		"rb" => "ruby",
		"rs" => "rust",
		"go" => "go",
		"java" => "java",
		"c" | "h" => "c",
		"cpp" | "hpp" => "cpp",
		"cs" => "csharp",
		"php" => "php",
		"sh" | "bash" | "zsh" => "bash",
		"sql" => "sql",
		"html" => "html",
		"css" => "css",
		"scss" => "scss",
		"json" => "json",
		"yaml" | "yml" => "yaml",
		"xml" => "xml",
		"md" => "markdown",
		"dockerfile" => "dockerfile",
		_ => return None,
	};
	Some(mapped)
}

/// `parseLineNumber` + `formatReadLineRange`: `:start` / `:start-end`. JSON
/// null counts as absent (JS `== null`).
fn format_read_line_range(
	offset: Option<&serde_json::Value>,
	limit: Option<&serde_json::Value>,
) -> String {
	let as_number = |value: &serde_json::Value| -> Option<f64> {
		match value {
			serde_json::Value::Number(n) => n.as_f64(),
			serde_json::Value::String(s) if !s.trim().is_empty() => s.trim().parse().ok(),
			_ => None,
		}
	};
	let offset = offset.filter(|value| !value.is_null());
	let limit = limit.filter(|value| !value.is_null());
	if offset.is_none() && limit.is_none() {
		return String::new();
	}
	let start = match offset.map(as_number) {
		None => 1.0,
		Some(Some(value)) => value,
		Some(None) => return String::new(),
	};
	if !start.is_finite() {
		return String::new();
	}
	let Some(limit) = limit else {
		return format!(":{}", format_line_number(start));
	};
	let Some(count) = as_number(limit) else {
		return String::new();
	};
	let end = start + count - 1.0;
	if end != 0.0 {
		format!(":{}-{}", format_line_number(start), format_line_number(end))
	} else {
		format!(":{}", format_line_number(start))
	}
}

/// `args.a ?? args.b` + `str(value)`: first key with a string value wins;
/// missing/null keys fall through (JS `??`); a non-string non-null value in a
/// consulted position renders `[invalid arg]` (`None`).
fn arg_str_first(args: &serde_json::Value, keys: &[&str]) -> Option<String> {
	let mut saw_other = false;
	for key in keys {
		match args.get(key) {
			None | Some(serde_json::Value::Null) => continue,
			Some(serde_json::Value::String(text)) => return Some(text.clone()),
			Some(_) => saw_other = true,
		}
	}
	if saw_other { None } else { Some(String::new()) }
}

/// Integer line numbers render without a decimal point (JS `Number` semantics).
fn format_line_number(value: f64) -> String {
	if value.fract() == 0.0 {
		format!("{}", value as i64)
	} else {
		format!("{value}")
	}
}

/// Result of pairing tool results with their calls.
struct ToolResults<'a> {
	results: HashMap<&'a str, &'a pi_ai::ToolResultMessage>,
}

impl<'a> ToolResults<'a> {
	/// `findToolResult` (template.js): built over ALL entries, not just the
	/// rendered branch; first match wins.
	fn build(entries: &'a [SessionEntry]) -> Self {
		let mut results = HashMap::new();
		for entry in entries {
			if let EntryKind::Message { message } = &entry.kind
				&& let pi_ai::Message::ToolResult(result) = message.as_ref()
			{
				results
					.entry(result.tool_call_id.as_str())
					.or_insert(result);
			}
		}
		Self { results }
	}
}

// ---------------------------------------------------------------------------
// Entry rendering (template.js `renderEntry` pipeline)
// ---------------------------------------------------------------------------

const EXPAND_CLICK: &str =
	"onclick=\"if(window.getSelection().toString())return;this.classList.toggle('expanded')\"";

/// Text of a user message: string content passthrough, else text blocks
/// joined with `\n` (template.js renderEntry user branch).
fn user_text(message: &pi_ai::UserMessage) -> String {
	match &message.content {
		pi_ai::UserContent::Text(text) => text.clone(),
		pi_ai::UserContent::Blocks(blocks) => blocks
			.iter()
			.filter_map(|block| block.text_content())
			.collect::<Vec<_>>()
			.join("\n"),
	}
}

fn message_images_html(blocks: &[pi_ai::TextOrImage]) -> String {
	let images: Vec<String> = blocks
		.iter()
		.filter_map(|block| match block {
			pi_ai::TextOrImage::Image(image) => Some(format!(
				"<img src=\"data:{};base64,{}\" class=\"message-image\" />",
				escape_html(&image.mime_type),
				escape_html(&image.data),
			)),
			_ => None,
		})
		.collect();
	if images.is_empty() {
		String::new()
	} else {
		format!("<div class=\"message-images\">{}</div>", images.join(""))
	}
}

/// The `<div class="message-timestamp">` prefix (empty when no timestamp).
fn timestamp_html(entry: &SessionEntry) -> String {
	let ts = format_timestamp(entry.base.timestamp.as_deref());
	if ts.is_empty() {
		String::new()
	} else {
		format!("<div class=\"message-timestamp\">{ts}</div>")
	}
}

fn entry_dom_id(entry: &SessionEntry) -> String {
	format!(
		"entry-{}",
		escape_html(entry.base.id.as_deref().unwrap_or(""))
	)
}

/// `formatExpandableOutput` (template.js). The per-line wrapper uses the
/// ANSI-rendered `<div class="ansi-line">` form; `lang` keeps the
/// `<pre><code class="hljs">` structure without highlighting.
fn format_expandable_output(text: &str, max_lines: usize, lang: Option<&str>) -> String {
	let text = replace_tabs(text);
	let lines: Vec<&str> = text.split('\n').collect();
	let remaining = lines.len().saturating_sub(max_lines);
	let display = &lines[..lines.len().min(max_lines)];

	if let Some(_lang) = lang {
		// The hljs language would select highlighting upstream; the port
		// keeps the `<pre><code class="hljs">` shape without highlighting.
		let code_block = |content: &str| {
			format!(
				"<pre><code class=\"hljs\">{}</code></pre>",
				escape_html(content)
			)
		};
		if remaining > 0 {
			let preview = display.join("\n");
			return format!(
				"<div class=\"tool-output expandable\" {EXPAND_CLICK}>\
<div class=\"output-preview\">{}<div class=\"expand-hint\">... ({remaining} more lines)</div></div>\
<div class=\"output-full\">{}</div></div>",
				code_block(&preview),
				code_block(&text),
			);
		}
		return format!("<div class=\"tool-output\">{}</div>", code_block(&text));
	}

	if remaining > 0 {
		format!(
			"<div class=\"tool-output expandable\" {EXPAND_CLICK}>\
<div class=\"output-preview\">{}<div class=\"expand-hint\">... ({remaining} more lines)</div></div>\
<div class=\"output-full\">{}</div></div>",
			ansi_lines_to_html(display),
			ansi_lines_to_html(&lines),
		)
	} else {
		format!(
			"<div class=\"tool-output\">{}</div>",
			ansi_lines_to_html(&lines)
		)
	}
}

/// `getResultText`: text blocks of a tool result joined with `\n`.
fn result_text(result: &pi_ai::ToolResultMessage) -> String {
	result
		.content
		.iter()
		.filter_map(|block| block.text_content())
		.collect::<Vec<_>>()
		.join("\n")
}

/// `renderToolCall` (template.js:1006-1177) for the builtin tool shapes
/// (`bash`/`read`/`write`/`edit`/`ls`) plus the JSON fallback for everything
/// else. The `renderedTools` pre-render path and codemode `nestedCalls`
/// records have no pi-rs renderer source and are omitted.
fn render_tool_call(call: &pi_ai::ToolCall, result: Option<&pi_ai::ToolResultMessage>) -> String {
	let status = match result {
		Some(result) if result.is_error => "error",
		Some(_) => "success",
		None => "pending",
	};
	let tool_dom_id = format!("tool-call-{}", escape_html(&call.id));
	let mut html = format!("<div class=\"tool-execution {status}\" id=\"{tool_dom_id}\">");
	let args = &call.arguments;
	let name = call.name.as_str();
	let invalid_arg = "<span class=\"tool-error\">[invalid arg]</span>";

	match name {
		"bash" => {
			let command = arg_str(args, "command");
			let display = match &command {
				None => invalid_arg.to_string(),
				Some(command) => {
					if command.is_empty() {
						escape_html("...")
					} else {
						escape_html(command)
					}
				}
			};
			html.push_str(&format!("<div class=\"tool-command\">$ {display}</div>"));
			if let Some(result) = result {
				let output = result_text(result).trim().to_string();
				if !output.is_empty() {
					html.push_str(&format_expandable_output(&output, 5, None));
				}
			}
		}
		"read" => {
			let file_path = arg_str_first(args, &["file_path", "path"]).unwrap_or_default();
			let range = format_read_line_range(args.get("offset"), args.get("limit"));
			let mut path_html = escape_html(&shorten_path(&file_path));
			if !range.is_empty() {
				path_html.push_str(&format!(
					"<span class=\"line-numbers\">{}</span>",
					escape_html(&range)
				));
			}
			html.push_str(&format!(
				"<div class=\"tool-header\"><span class=\"tool-name\">read</span> <span class=\"tool-path\">{path_html}</span></div>"
			));
			if let Some(result) = result {
				let output = result_text(result);
				let lang = language_from_path(&file_path);
				if !output.is_empty() {
					html.push_str(&format_expandable_output(&output, 10, lang));
				}
			}
		}
		"write" => {
			let file_path = arg_str_first(args, &["file_path", "path"]).unwrap_or_default();
			let content = arg_str(args, "content");
			let mut header = format!(
				"<div class=\"tool-header\"><span class=\"tool-name\">write</span> <span class=\"tool-path\">{}</span>",
				escape_html(&shorten_path(&file_path))
			);
			if let Some(content) = &content {
				let line_count = content.split('\n').count();
				if line_count > 10 {
					header.push_str(&format!(
						" <span class=\"line-count\">({line_count} lines)</span>"
					));
				}
			}
			header.push_str("</div>");
			html.push_str(&header);

			match &content {
				None => html.push_str(
					"<div class=\"tool-error\">[invalid content arg - expected string]</div>",
				),
				Some(content) if !content.is_empty() => {
					let lang = language_from_path(&file_path);
					html.push_str(&format_expandable_output(content, 10, lang));
				}
				Some(_) => {}
			}
			if let Some(result) = result {
				let output = result_text(result).trim().to_string();
				if !output.is_empty() {
					html.push_str(&format!(
						"<div class=\"tool-output\"><div>{}</div></div>",
						escape_html(&output)
					));
				}
			}
		}
		"edit" => {
			let file_path = arg_str_first(args, &["file_path", "path"]).unwrap_or_default();
			html.push_str(&format!(
				"<div class=\"tool-header\"><span class=\"tool-name\">edit</span> <span class=\"tool-path\">{}</span></div>",
				escape_html(&shorten_path(&file_path))
			));
			let diff = result
				.and_then(|result| result.details.as_ref())
				.and_then(|details| details.get("diff"))
				.and_then(serde_json::Value::as_str);
			if let Some(diff) = diff {
				html.push_str("<div class=\"tool-diff\">");
				for line in replace_tabs(diff).split('\n') {
					let class = if line.starts_with('+') {
						"diff-added"
					} else if line.starts_with('-') {
						"diff-removed"
					} else {
						"diff-context"
					};
					html.push_str(&format!(
						"<div class=\"{class}\">{}</div>",
						escape_html(line)
					));
				}
				html.push_str("</div>");
			} else if let Some(result) = result {
				let output = result_text(result).trim().to_string();
				if !output.is_empty() {
					html.push_str(&format!(
						"<div class=\"tool-output\"><pre>{}</pre></div>",
						escape_html(&output)
					));
				}
			}
		}
		"ls" => {
			let dir_path = arg_str(args, "path").unwrap_or_default();
			let mut path_html = escape_html(&shorten_path(if dir_path.is_empty() {
				"."
			} else {
				&dir_path
			}));
			if let Some(limit) = args.get("limit") {
				path_html.push_str(&format!(
					" <span class=\"line-count\">(limit {})</span>",
					escape_html(&limit.to_string())
				));
			}
			html.push_str(&format!(
				"<div class=\"tool-header\"><span class=\"tool-name\">ls</span> <span class=\"tool-path\">{path_html}</span></div>"
			));
			if let Some(result) = result {
				let output = result_text(result).trim().to_string();
				if !output.is_empty() {
					html.push_str(&format_expandable_output(&output, 20, None));
				}
			}
		}
		_ => {
			// JSON fallback (template.js default branch).
			html.push_str(&format!(
				"<div class=\"tool-header\"><span class=\"tool-name\">{}</span></div>",
				escape_html(name)
			));
			let pretty = serde_json::to_string_pretty(args).unwrap_or_default();
			html.push_str(&format!(
				"<div class=\"tool-output\"><pre>{}</pre></div>",
				escape_html(&pretty)
			));
			if let Some(result) = result {
				let output = result_text(result);
				if !output.is_empty() {
					html.push_str(&format_expandable_output(&output, 10, None));
				}
			}
		}
	}

	html.push_str("</div>");
	html
}

/// `getSystemMessageText`: content plus non-null section values.
fn system_message_text(message: &pi_ai::SystemMessage) -> String {
	let mut parts: Vec<String> = Vec::new();
	match &message.content {
		pi_ai::SystemContent::Text(text) if !text.is_empty() => parts.push(text.clone()),
		pi_ai::SystemContent::Blocks(blocks) => {
			let text = blocks
				.iter()
				.map(|block| block.text.as_str())
				.collect::<Vec<_>>()
				.join("");
			if !text.is_empty() {
				parts.push(text);
			}
		}
		_ => {}
	}
	if let Some(sections) = &message.sections {
		for value in sections.values().flatten() {
			if !value.is_empty() {
				parts.push(value.clone());
			}
		}
	}
	parts.join("\n\n")
}

/// `renderSystemPromptBlock` (template.js:1556): 10-line preview above the
/// fold, full text behind the expand toggle.
fn render_system_prompt_block(title: &str, text: &str) -> String {
	let lines: Vec<&str> = text.split('\n').collect();
	if lines.len() > 10 {
		let preview = lines[..10].join("\n");
		let remaining = lines.len() - 10;
		return format!(
			"<div class=\"system-prompt expandable\" {EXPAND_CLICK}>\
<div class=\"system-prompt-header\">{}</div>\
<div class=\"system-prompt-preview\">{}</div>\
<div class=\"system-prompt-expand-hint\">... ({remaining} more lines, click to expand)</div>\
<div class=\"system-prompt-full\">{}</div></div>",
			escape_html(title),
			escape_html(&preview),
			escape_html(text),
		);
	}
	format!(
		"<div class=\"system-prompt\"><div class=\"system-prompt-header\">{}</div>\
<div class=\"system-prompt-full\" style=\"display: block\">{}</div></div>",
		escape_html(title),
		escape_html(text),
	)
}

/// Whether any earlier entry on the path carries system state (system message
/// or a compaction that shipped one) — `carriesSystemState` upstream, so this
/// system message renders as a prompt (leading) or an update.
fn path_has_system_state_before(path: &[SessionEntry], index: usize) -> bool {
	path[..index].iter().any(|entry| match &entry.kind {
		EntryKind::Message { message } => matches!(message.as_ref(), pi_ai::Message::System(_)),
		EntryKind::Compaction { system_message, .. } => system_message.is_some(),
		_ => false,
	})
}

/// Render one entry of the branch path; `""` for entries that render nothing
/// of their own (tool results render inside their call block; label entries
/// only feed the tree).
fn render_entry(
	entry: &SessionEntry,
	index: usize,
	path: &[SessionEntry],
	tools: &ToolResults,
) -> String {
	let ts_html = timestamp_html(entry);
	let dom_id = entry_dom_id(entry);

	match &entry.kind {
		EntryKind::Message { message } => match message.as_ref() {
			pi_ai::Message::User(message) => {
				let text = user_text(message);
				let mut html = format!("<div class=\"user-message\" id=\"{dom_id}\">{ts_html}");
				if let pi_ai::UserContent::Blocks(blocks) = &message.content {
					html.push_str(&message_images_html(blocks));
				}
				if !text.trim().is_empty() {
					html.push_str(&format!(
						"<div class=\"markdown-content\">{}</div>",
						escape_html(&text)
					));
				}
				html.push_str("</div>");
				html
			}
			pi_ai::Message::Assistant(message) => {
				let mut html =
					format!("<div class=\"assistant-message\" id=\"{dom_id}\">{ts_html}");
				for block in &message.content {
					match block {
						pi_ai::AssistantContent::Text(text) if !text.text.trim().is_empty() => {
							html.push_str(&format!(
								"<div class=\"assistant-text markdown-content\">{}</div>",
								escape_html(&text.text)
							));
						}
						pi_ai::AssistantContent::Thinking(thinking)
							if !thinking.thinking.trim().is_empty() =>
						{
							html.push_str(&format!(
								"<div class=\"thinking-block\"><div class=\"thinking-text\">{}</div>\
<div class=\"thinking-collapsed\">Thinking ...</div></div>",
								escape_html(&thinking.thinking)
							));
						}
						_ => {}
					}
				}
				for block in &message.content {
					if let pi_ai::AssistantContent::ToolCall(call) = block {
						let result = tools.results.get(call.id.as_str()).copied();
						html.push_str(&render_tool_call(call, result));
					}
				}
				match message.stop_reason {
					pi_ai::StopReason::Aborted => {
						html.push_str("<div class=\"error-text\">Aborted</div>")
					}
					pi_ai::StopReason::Error => html.push_str(&format!(
						"<div class=\"error-text\">Error: {}</div>",
						escape_html(message.error_message.as_deref().unwrap_or("Unknown error"))
					)),
					_ => {}
				}
				html.push_str("</div>");
				html
			}
			// Tool results render inside their tool-call block (upstream
			// returns '' here and resolves the result by toolCallId).
			pi_ai::Message::ToolResult(_) => String::new(),
			pi_ai::Message::System(message) => {
				let text = system_message_text(message);
				let mut html = String::new();
				let leading = !path_has_system_state_before(path, index);
				if leading {
					if !text.is_empty() {
						html.push_str(&render_system_prompt_block("System Prompt", &text));
					}
					html.push_str(&render_tools_list("Available Tools", &message.tools_added));
				} else {
					// `describeSystemUpdate` suffix (tools added/removed
					// summary) is not replicated; the title stays plain.
					if !text.is_empty() {
						html.push_str(&render_system_prompt_block("System Prompt Update", &text));
					}
					html.push_str(&render_tools_list("Tools Added", &message.tools_added));
					if let Some(removed) = &message.tools_removed
						&& !removed.is_empty()
					{
						let items: String = removed
							.iter()
							.map(|tool| {
								format!(
									"<div class=\"tool-item\"><span class=\"tool-item-name\">{}</span></div>",
									escape_html(&tool.name)
								)
							})
							.collect();
						html.push_str(&format!(
							"<div class=\"tools-list\"><div class=\"tools-header\">Tools Removed</div>\
<div class=\"tools-content\">{items}</div></div>"
						));
					}
				}
				if html.is_empty() {
					String::new()
				} else {
					format!("<div class=\"system-entry\" id=\"{dom_id}\">{ts_html}{html}</div>")
				}
			}
		},
		EntryKind::ModelChange { provider, model_id } => format!(
			"<div class=\"model-change\" id=\"{dom_id}\">{ts_html}Switched to model: \
<span class=\"model-name\">{}/{}</span></div>",
			escape_html(provider),
			escape_html(model_id)
		),
		EntryKind::Compaction {
			summary,
			tokens_before,
			..
		} => format!(
			"<div class=\"compaction\" id=\"{dom_id}\" {EXPAND_CLICK}>\
<div class=\"compaction-label\">[compaction]</div>\
<div class=\"compaction-collapsed\">Compacted from {tokens_before} tokens</div>\
<div class=\"compaction-content\"><strong>Compacted from {tokens_before} tokens</strong>\n\n{}</div></div>",
			escape_html(summary)
		),
		EntryKind::BranchSummary { summary, .. } => format!(
			"<div class=\"branch-summary\" id=\"{dom_id}\">{ts_html}\
<div class=\"branch-summary-header\">Branch Summary</div>\
<div class=\"markdown-content\">{}</div></div>",
			escape_html(summary)
		),
		EntryKind::CustomMessage {
			custom_type,
			content,
			display,
			..
		} => {
			let hidden = !*display;
			let content_text = match content {
				serde_json::Value::String(text) => text.clone(),
				other => other.to_string(),
			};
			format!(
				"<div class=\"hook-message{}\" id=\"{dom_id}\">{ts_html}\
<div class=\"hook-type\">[{}]{}</div>\
<div class=\"markdown-content\">{}</div></div>",
				if hidden { " hook-message-hidden" } else { "" },
				escape_html(custom_type),
				if hidden { " · Hidden in terminal" } else { "" },
				escape_html(&content_text)
			)
		}
		// `thinking_level_change`/`usage`/`custom`/`context_edit`/`label`/
		// `session_info`/unknown entries have no transcript render upstream.
		_ => String::new(),
	}
}

/// `renderToolsList` (template.js:1575): tool name + description rows with
/// parameter summaries.
fn render_tools_list(title: &str, tools: &Option<Vec<pi_ai::Tool>>) -> String {
	let Some(tools) = tools else {
		return String::new();
	};
	if tools.is_empty() {
		return String::new();
	}
	let items: String = tools
		.iter()
		.map(|tool| {
			format!(
				"<div class=\"tool-item\"><span class=\"tool-item-name\">{}</span> - \
<span class=\"tool-item-desc\">{}</span></div>",
				escape_html(&tool.name),
				escape_html(&tool.description)
			)
		})
		.collect();
	format!(
		"<div class=\"tools-list\"><div class=\"tools-header\">{}</div>\
<div class=\"tools-content\">{items}</div></div>",
		escape_html(title)
	)
}

/// `computeStats` + `renderHeader` (template.js): the stats bar over ALL
/// entries. The help bar / toggle buttons (viewer JS) are omitted.
fn render_header(header: Option<&SessionHeader>, entries: &[SessionEntry]) -> String {
	let mut user_messages = 0u64;
	let mut assistant_messages = 0u64;
	let mut tool_results = 0u64;
	let mut compactions = 0u64;
	let mut branch_summaries = 0u64;
	let mut tool_calls = 0u64;
	let mut tokens = (0u64, 0u64, 0u64, 0u64);
	let mut cost_total = 0f64;
	let mut models: Vec<String> = Vec::new();

	for entry in entries {
		if let EntryKind::Message { message } = &entry.kind {
			match message.as_ref() {
				pi_ai::Message::User(_) => user_messages += 1,
				pi_ai::Message::ToolResult(_) => tool_results += 1,
				pi_ai::Message::Assistant(assistant) => {
					assistant_messages += 1;
					let model = format!("{}/{}", assistant.provider, assistant.model);
					if !models.contains(&model) {
						models.push(model);
					}
					let usage = &assistant.usage;
					tokens.0 += usage.input;
					tokens.1 += usage.output;
					tokens.2 += usage.cache_read;
					tokens.3 += usage.cache_write;
					let component_total = usage.cost.input
						+ usage.cost.output
						+ usage.cost.cache_read.unwrap_or(0.0)
						+ usage.cost.cache_write.unwrap_or(0.0);
					// Provider-reported totals can differ from catalog-estimated
					// components; fall back to the component sum (upstream
					// comment preserved).
					cost_total += usage.cost.total.unwrap_or(component_total);
					tool_calls += assistant
						.content
						.iter()
						.filter(|block| matches!(block, pi_ai::AssistantContent::ToolCall(_)))
						.count() as u64;
				}
				pi_ai::Message::System(_) => {}
			}
		} else if matches!(entry.kind, EntryKind::Compaction { .. }) {
			compactions += 1;
		} else if matches!(entry.kind, EntryKind::BranchSummary { .. }) {
			branch_summaries += 1;
		}
	}

	let mut token_parts: Vec<String> = Vec::new();
	if tokens.0 > 0 {
		token_parts.push(format!("↑{}", format_tokens(tokens.0)));
	}
	if tokens.1 > 0 {
		token_parts.push(format!("↓{}", format_tokens(tokens.1)));
	}
	if tokens.2 > 0 {
		token_parts.push(format!("R{}", format_tokens(tokens.2)));
	}
	if tokens.3 > 0 {
		token_parts.push(format!("W{}", format_tokens(tokens.3)));
	}
	let mut msg_parts: Vec<String> = Vec::new();
	if user_messages > 0 {
		msg_parts.push(format!("{user_messages} user"));
	}
	if assistant_messages > 0 {
		msg_parts.push(format!("{assistant_messages} assistant"));
	}
	if tool_results > 0 {
		msg_parts.push(format!("{tool_results} tool results"));
	}
	if compactions > 0 {
		msg_parts.push(format!("{compactions} compactions"));
	}
	if branch_summaries > 0 {
		msg_parts.push(format!("{branch_summaries} branch summaries"));
	}

	let session_id = header
		.map(|header| escape_html(&header.id))
		.unwrap_or_else(|| "unknown".into());
	let date = header
		.map(|header| escape_html(&header.timestamp))
		.unwrap_or_else(|| "unknown".into());
	format!(
		"<div class=\"header\"><h1>Session: {session_id}</h1><div class=\"header-info\">\
<div class=\"info-item\"><span class=\"info-label\">Date:</span><span class=\"info-value\">{date}</span></div>\
<div class=\"info-item\"><span class=\"info-label\">Models:</span><span class=\"info-value\">{}</span></div>\
<div class=\"info-item\"><span class=\"info-label\">Messages:</span><span class=\"info-value\">{}</span></div>\
<div class=\"info-item\"><span class=\"info-label\">Tool Calls:</span><span class=\"info-value\">{tool_calls}</span></div>\
<div class=\"info-item\"><span class=\"info-label\">Tokens:</span><span class=\"info-value\">{}</span></div>\
<div class=\"info-item\"><span class=\"info-label\">Cost:</span><span class=\"info-value\">${cost_total:.3}</span></div>\
</div></div>",
		escape_html(&models.join(", ")),
		if msg_parts.is_empty() {
			"0".to_string()
		} else {
			escape_html(&msg_parts.join(", "))
		},
		if token_parts.is_empty() {
			"0".to_string()
		} else {
			escape_html(&token_parts.join(" "))
		},
	)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// `generateHtml` + the template.js render pass, server-side: renders the
/// branch from `leaf_id` (upstream `getPath(currentLeafId)`) with the stats
/// header over all `entries` (upstream `computeStats(entries)`).
///
/// Pure: callers hand in the session's public projection
/// (`session_manager::{SessionHeader, SessionEntry, build_session_path}`) and
/// get the document back; nothing touches the filesystem.
pub fn render_session_html(
	header: Option<&SessionHeader>,
	entries: &[SessionEntry],
	leaf_id: Option<&str>,
) -> String {
	let path = build_session_path(entries, leaf_id);
	let tools = ToolResults::build(entries);

	let messages: String = path
		.iter()
		.enumerate()
		.map(|(index, entry)| render_entry(entry, index, &path, &tools))
		.collect();

	let html = format!(
		"<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"UTF-8\">\n  \
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n  \
<title>Session Export</title>\n  <style>\n{TEMPLATE_CSS}  </style>\n</head>\n<body>\n  \
<main id=\"content\">\n    <div id=\"header-container\">{}</div>\n    \
<div id=\"messages\">{messages}</div>\n  </main>\n</body>\n</html>\n",
		render_header(header, entries),
	);
	html
}

/// Export a session directory's log to HTML — `exportSessionToHtml`
/// (`export-html/index.ts:238`) with the output-path derivation of
/// `exportToHtml`: an explicit `output_path` (relative paths resolve against
/// the process cwd) wins, otherwise `<app>-session-<session-id>.html` in the
/// cwd. Upstream derives the name from the `<uuid>.jsonl` log file; pi-rs
/// stores logs as `<session-dir>/session.jsonl`, so the directory name (the
/// session id) stands in. `APP_NAME` is `pi-rs` (upstream: `pi`).
pub fn write_session_html(
	session_dir: &Path,
	output_path: Option<&Path>,
) -> anyhow::Result<PathBuf> {
	let session_file = session_dir.join("session.jsonl");
	if !session_file.exists() {
		// Upstream error text ("Nothing to export yet - start a conversation
		// first" / "Cannot export in-memory session to HTML").
		anyhow::bail!("Nothing to export yet - start a conversation first");
	}
	let (header, entries) = load_session_file(&session_file);
	// Leaf = last id-bearing entry in file order (SessionManager::open's
	// `index_entry` rule); upstream reads the manager's live `leafId`.
	let leaf_id = entries
		.iter()
		.rev()
		.find_map(|entry| entry.id().map(str::to_owned));
	let html = render_session_html(header.as_ref(), &entries, leaf_id.as_deref());

	let output = match output_path {
		Some(path) => {
			if path.is_absolute() {
				path.to_path_buf()
			} else {
				std::env::current_dir()?.join(path)
			}
		}
		None => {
			let basename = session_dir
				.file_name()
				.map(|name| name.to_string_lossy().into_owned())
				.unwrap_or_else(|| "session".into());
			std::env::current_dir()?.join(format!("{APP_NAME}-session-{basename}.html"))
		}
	};
	std::fs::write(&output, html)?;
	Ok(output)
}

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

/// Condensed subset of `template.css` covering the classes this renderer
/// emits, with the upstream default dark theme hardcoded (module docs list
/// the provenance of each value). Class names and visual rules mirror
/// template.css; rules for viewer-only chrome (sidebar, modals, buttons) are
/// dropped.
const TEMPLATE_CSS: &str = r#"    :root {
      --text: #dee0e1;
      --muted: #9da5a9;
      --dim: #7e888e;
      --accent: #a798d7;
      --success: #68b78d;
      --error: #ea7f81;
      --warning: #cd9a22;
      --thinkingText: #96a0a4;
      --userMessageBg: #213b49;
      --toolPendingBg: #34383a;
      --toolSuccessBg: #254131;
      --toolErrorBg: #5b282a;
      --exportPageBg: #21252c;
      --exportCardBg: #282c34;
      --exportInfoBg: #4e2f1b;
      --body-bg: #21252c;
      --container-bg: #282c34;
      --info-bg: #4e2f1b;
    }

    * { margin: 0; padding: 0; box-sizing: border-box; }

    body {
      font-family: ui-monospace, 'Cascadia Code', 'Source Code Pro', Menlo, Consolas, 'DejaVu Sans Mono', monospace;
      font-size: 12px;
      line-height: 18px;
      color: var(--text);
      background: var(--body-bg);
    }

    #content { max-width: 900px; margin: 0 auto; padding: 24px 16px 64px; }

    .header { background: var(--container-bg); border: 1px solid var(--dim); border-radius: 6px; padding: 12px 16px; margin-bottom: 16px; }
    .header h1 { font-size: 14px; margin-bottom: 8px; }
    .header-info { display: flex; flex-wrap: wrap; gap: 4px 16px; }
    .info-label { color: var(--muted); }
    .info-value { color: var(--text); }

    .message-timestamp { color: var(--muted); font-size: 10px; margin-bottom: 2px; }
    .user-message { background: var(--userMessageBg); border-radius: 6px; padding: 8px 12px; margin: 12px 0; white-space: pre-wrap; word-break: break-word; }
    .assistant-message { border-radius: 6px; padding: 8px 12px; margin: 12px 0; }
    .markdown-content { white-space: pre-wrap; word-break: break-word; }
    .assistant-text { white-space: pre-wrap; word-break: break-word; }
    .thinking-block { border-left: 2px solid var(--dim); padding-left: 8px; margin: 6px 0; color: var(--thinkingText); }
    .thinking-text { white-space: pre-wrap; word-break: break-word; display: none; }
    .thinking-block.expanded .thinking-text { display: block; }
    .thinking-collapsed { font-style: italic; }
    .thinking-block.expanded .thinking-collapsed { display: none; }
    .error-text { color: var(--error); margin-top: 6px; white-space: pre-wrap; }

    .tool-execution { background: var(--toolPendingBg); border-radius: 6px; padding: 6px 10px; margin: 8px 0; }
    .tool-execution.success { background: var(--toolSuccessBg); }
    .tool-execution.error { background: var(--toolErrorBg); }
    .tool-command { color: var(--text); white-space: pre-wrap; word-break: break-all; }
    .tool-header { color: var(--text); }
    .tool-name { font-weight: bold; }
    .tool-path { color: var(--muted); word-break: break-all; }
    .line-numbers, .line-count { color: var(--muted); }
    .tool-error { color: var(--error); }
    .tool-output { color: var(--muted); margin-top: 4px; word-break: break-word; }
    .tool-output pre { white-space: pre-wrap; word-break: break-word; }
    .tool-output.expandable .output-full { display: none; }
    .tool-output.expandable.expanded .output-full { display: block; }
    .tool-output.expandable.expanded .output-preview { display: none; }
    .expand-hint { color: var(--dim); font-style: italic; }
    .ansi-line { white-space: pre-wrap; }

    .tool-diff { margin-top: 4px; white-space: pre-wrap; }
    .diff-added { color: var(--success); }
    .diff-removed { color: var(--error); }
    .diff-context { color: var(--muted); }

    .model-change { color: var(--muted); margin: 10px 0; }
    .model-name { color: var(--accent); }
    .compaction { border: 1px solid var(--dim); border-radius: 6px; padding: 8px 12px; margin: 12px 0; cursor: pointer; }
    .compaction-label { color: var(--warning); }
    .compaction-collapsed { color: var(--muted); }
    .compaction-content { display: none; white-space: pre-wrap; }
    .compaction.expanded .compaction-content { display: block; }
    .compaction.expanded .compaction-collapsed { display: none; }
    .branch-summary { border-left: 2px solid var(--warning); padding-left: 8px; margin: 12px 0; }
    .branch-summary-header { color: var(--warning); font-weight: bold; }
    .hook-message { border-left: 2px solid var(--accent); padding-left: 8px; margin: 12px 0; }
    .hook-type { color: var(--muted); }
    .hook-message-hidden { display: none; }

    .system-entry { margin: 12px 0; }
    .system-prompt { background: var(--container-bg); border: 1px solid var(--dim); border-radius: 6px; padding: 8px 12px; }
    .system-prompt-header { color: var(--muted); font-weight: bold; margin-bottom: 4px; }
    .system-prompt-preview { white-space: pre-wrap; }
    .system-prompt-full { display: none; white-space: pre-wrap; }
    .system-prompt.expandable.expanded .system-prompt-full { display: block; }
    .system-prompt.expandable.expanded .system-prompt-preview { display: none; }
    .system-prompt-expand-hint { color: var(--dim); font-style: italic; }
    .tools-list { margin-top: 8px; }
    .tools-header { color: var(--muted); font-weight: bold; }
    .tool-item { color: var(--muted); }
    .tool-item-name { color: var(--text); }

    .message-images { margin: 6px 0; }
    .message-image { max-width: 100%; border-radius: 4px; }
"#;

#[cfg(test)]
mod tests {
	use super::*;
	use crate::core::session_manager::{EntryBase, SessionEntry};
	use serde_json::json;

	fn entry(kind: EntryKind) -> SessionEntry {
		SessionEntry {
			base: EntryBase {
				id: Some("e1".into()),
				parent_id: None,
				timestamp: Some("2026-01-02T03:04:05.000Z".into()),
			},
			kind,
		}
	}

	fn user_entry(text: &str) -> SessionEntry {
		entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::User(pi_ai::UserMessage {
				content: pi_ai::UserContent::Blocks(vec![pi_ai::TextOrImage::text(text)]),
				timestamp: 0,
			})),
		})
	}

	// -- ANSI → HTML --------------------------------------------------------

	#[test]
	fn ansi_bold_and_color_map_to_inline_spans() {
		assert_eq!(
			ansi_to_html("\x1b[1;31mred\x1b[0m plain"),
			"<span style=\"color:#800000;font-weight:bold\">red</span> plain"
		);
	}

	#[test]
	fn ansi_reset_splits_spans_and_restores_defaults() {
		let html = ansi_to_html("\x1b[32mgreen\x1b[0m\x1b[1mbold");
		assert_eq!(
			html,
			"<span style=\"color:#008000\">green</span><span style=\"font-weight:bold\">bold</span>"
		);
	}

	#[test]
	fn ansi_256_color_cube_and_grayscale() {
		// Cube 16 → #000000, 231 → #ffffff (component 5 → 55+5*40=255).
		assert_eq!(color_256_to_hex(16), "#000000");
		assert_eq!(color_256_to_hex(231), "#ffffff");
		assert_eq!(color_256_to_hex(196), "#ff0000");
		// Grayscale 232 → #080808, 255 → #eeeeee.
		assert_eq!(color_256_to_hex(232), "#080808");
		assert_eq!(color_256_to_hex(255), "#eeeeee");
		assert_eq!(
			ansi_to_html("\x1b[38;5;196mred256\x1b[49m"),
			// [49m clears only the background; the fg survives, so upstream
			// closes and reopens an identical span.
			"<span style=\"color:#ff0000\">red256</span><span style=\"color:#ff0000\"></span>"
		);
	}

	#[test]
	fn ansi_truecolor_rgb_and_text_styles() {
		assert_eq!(
			ansi_to_html("\x1b[38;2;12;34;56mrgb\x1b[0m"),
			"<span style=\"color:rgb(12,34,56)\">rgb</span>"
		);
		assert_eq!(
			ansi_to_html("\x1b[2;3;4mdiu\x1b[22mlater"),
			"<span style=\"opacity:0.6;font-style:italic;text-decoration:underline\">diu</span>\
<span style=\"font-style:italic;text-decoration:underline\">later</span>"
		);
	}

	#[test]
	fn ansi_escapes_html_and_ignores_unknown_codes() {
		assert_eq!(
			ansi_to_html("\x1b[97m<b>&</b>\x1b[0m"),
			"<span style=\"color:#ffffff\">&lt;b&gt;&amp;&lt;/b&gt;</span>"
		);
		// Unknown SGR (e.g. 7 reverse) leaves styling unchanged.
		assert_eq!(ansi_to_html("\x1b[7mx"), "x");
		// Incomplete sequences are passed through as text.
		assert_eq!(ansi_to_html("\x1b[31"), "\x1b[31");
	}

	#[test]
	fn ansi_empty_lines_become_nbsp_in_line_wrapping() {
		assert_eq!(
			ansi_lines_to_html(&["one", "", "two"]),
			"<div class=\"ansi-line\">one</div><div class=\"ansi-line\">&nbsp;</div>\
<div class=\"ansi-line\">two</div>"
		);
	}

	// -- Escaping -----------------------------------------------------------

	#[test]
	fn message_content_is_html_escaped() {
		let mut assistant = entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::Assistant(pi_ai::AssistantMessage {
				content: vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
					text: "<img src=x onerror=boom>".into(),
					text_signature: None,
				})],
				api: "mock".into(),
				provider: "mock".into(),
				model: "mock-echo".into(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: pi_ai::Usage::default(),
				stop_reason: pi_ai::StopReason::Stop,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: 0,
				duration_ms: None,
			})),
		});
		assistant.base.id = Some("a1".into());
		assistant.base.parent_id = Some("u1".into());
		let mut user = user_entry("<script>alert('x&y')</script>");
		user.base.id = Some("u1".into());
		let entries = vec![user, assistant];
		let html = render_session_html(None, &entries, Some("a1"));
		assert!(html.contains("&lt;script&gt;alert(&#39;x&amp;y&#39;)&lt;/script&gt;"));
		assert!(html.contains("&lt;img src=x onerror=boom&gt;"));
		assert!(!html.contains("<script>alert"));
		assert!(!html.contains("<img src=x"));
	}

	// -- Golden structure fragments ------------------------------------------

	#[test]
	fn user_and_assistant_messages_render_upstream_dom_shapes() {
		let mut assistant = entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::Assistant(pi_ai::AssistantMessage {
				content: vec![
					pi_ai::AssistantContent::Thinking(pi_ai::ThinkingContent {
						thinking: "pondering".into(),
						thinking_signature: None,
						redacted: None,
					}),
					pi_ai::AssistantContent::Text(pi_ai::TextContent {
						text: "hello".into(),
						text_signature: None,
					}),
				],
				api: "mock".into(),
				provider: "mock".into(),
				model: "mock-echo".into(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: pi_ai::Usage {
					input: 900,
					output: 1200,
					..pi_ai::Usage::default()
				},
				stop_reason: pi_ai::StopReason::Stop,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: 0,
				duration_ms: None,
			})),
		});
		assistant.base.id = Some("a1".into());
		assistant.base.parent_id = Some("e1".into());
		let entries = vec![user_entry("hi"), assistant];
		let html = render_session_html(None, &entries, Some("a1"));
		assert!(html.contains("<!DOCTYPE html>"));
		assert!(html.contains("<title>Session Export</title>"));
		assert!(
			html.contains("<div class=\"user-message\" id=\"entry-e1\">"),
			"user message div with entry id"
		);
		assert!(html.contains("<div class=\"message-timestamp\">03:04:05</div>"));
		assert!(html.contains("<div class=\"markdown-content\">hi</div>"));
		assert!(html.contains("<div class=\"assistant-message\" id=\"entry-a1\">"));
		assert!(html.contains("<div class=\"assistant-text markdown-content\">hello</div>"));
		assert!(html.contains("<div class=\"thinking-text\">pondering</div>"));
		// Stats header over all entries (formatTokens rules).
		assert!(html.contains("Session: unknown"));
		assert!(html.contains("1 user"));
		assert!(html.contains("1 assistant"));
		assert!(html.contains("↑900"), "sub-1k tokens stay raw");
		assert!(html.contains("↓1.2k"));
	}

	#[test]
	fn error_and_abort_stop_reasons_render_error_text() {
		let mut errored = user_entry("irrelevant");
		errored.base.id = Some("e2".into());
		errored.kind = EntryKind::Message {
			message: Box::new(pi_ai::Message::Assistant(pi_ai::AssistantMessage {
				content: vec![],
				api: "mock".into(),
				provider: "mock".into(),
				model: "mock-echo".into(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: pi_ai::Usage::default(),
				stop_reason: pi_ai::StopReason::Error,
				error_message: Some("rate limited <now>".into()),
				raw_stop_reason: None,
				end_turn: None,
				timestamp: 0,
				duration_ms: None,
			})),
		};
		let html = render_session_html(None, &[errored], Some("e2"));
		assert!(html.contains("<div class=\"error-text\">Error: rate limited &lt;now&gt;</div>"));

		let mut aborted = user_entry("x");
		aborted.base.id = Some("e3".into());
		aborted.kind = EntryKind::Message {
			message: Box::new(pi_ai::Message::Assistant(pi_ai::AssistantMessage {
				content: vec![],
				api: "mock".into(),
				provider: "mock".into(),
				model: "mock-echo".into(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: pi_ai::Usage::default(),
				stop_reason: pi_ai::StopReason::Aborted,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: 0,
				duration_ms: None,
			})),
		};
		let html = render_session_html(None, &[aborted], Some("e3"));
		assert!(html.contains("<div class=\"error-text\">Aborted</div>"));
	}

	// -- Tool call blocks -----------------------------------------------------

	fn assistant_with_call() -> (SessionEntry, pi_ai::ToolCall) {
		let call = pi_ai::ToolCall {
			id: "call-1".into(),
			name: "bash".into(),
			arguments: json!({"command": "ls -la"}),
			thought_signature: None,
			namespace: None,
			arguments_error: None,
		};
		let message = pi_ai::Message::Assistant(pi_ai::AssistantMessage {
			content: vec![pi_ai::AssistantContent::ToolCall(call.clone())],
			api: "mock".into(),
			provider: "mock".into(),
			model: "mock-echo".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: pi_ai::Usage::default(),
			stop_reason: pi_ai::StopReason::ToolUse,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		});
		let mut assistant = entry(EntryKind::Message {
			message: Box::new(message),
		});
		assistant.base.id = Some("a1".into());
		(assistant, call)
	}

	fn result_entry(call_id: &str, text: &str, is_error: bool) -> SessionEntry {
		entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
				tool_call_id: call_id.into(),
				tool_name: "bash".into(),
				content: vec![pi_ai::TextOrImage::text(text)],
				details: None,
				usage: None,
				is_error,
				timestamp: 0,
				duration_ms: None,
			})),
		})
	}

	#[test]
	fn tool_call_renders_collapsed_output_with_expand_hint() {
		let (assistant, _) = assistant_with_call();
		let mut result = result_entry(
			"call-1",
			&("out\n".to_string() + &"line\n".repeat(8)),
			false,
		);
		result.base.id = Some("r1".into());
		result.base.parent_id = Some("a1".into());
		let entries = vec![user_entry("run"), assistant, result];
		let html = render_session_html(None, &entries, Some("r1"));

		assert!(html.contains("<div class=\"tool-execution success\" id=\"tool-call-call-1\">"));
		assert!(html.contains("<div class=\"tool-command\">$ ls -la</div>"));
		assert!(html.contains("<div class=\"tool-output expandable\""));
		assert!(html.contains("<div class=\"output-preview\">"));
		assert!(html.contains("<div class=\"expand-hint\">... (4 more lines)</div>"));
		assert!(html.contains("<div class=\"output-full\">"));
		// Tool result entries do not render standalone.
		assert!(!html.contains("tool-result-entry"));
	}

	#[test]
	fn tool_error_status_and_pending_without_result() {
		let (assistant, _) = assistant_with_call();
		let mut result = result_entry("call-1", "boom", true);
		result.base.id = Some("r1".into());
		result.base.parent_id = Some("a1".into());
		let html = render_session_html(None, &[assistant.clone(), result], Some("r1"));
		assert!(html.contains("<div class=\"tool-execution error\" id=\"tool-call-call-1\">"));

		let html = render_session_html(None, &[assistant], Some("a1"));
		assert!(html.contains("<div class=\"tool-execution pending\" id=\"tool-call-call-1\">"));
	}

	#[test]
	fn edit_diff_lines_classify_added_removed_context() {
		let (mut assistant, mut call) = assistant_with_call();
		call.name = "edit".into();
		call.arguments = json!({"file_path": "/tmp/x.rs"});
		assistant.kind = EntryKind::Message {
			message: Box::new(pi_ai::Message::Assistant(pi_ai::AssistantMessage {
				content: vec![pi_ai::AssistantContent::ToolCall(call)],
				api: "mock".into(),
				provider: "mock".into(),
				model: "mock-echo".into(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: pi_ai::Usage::default(),
				stop_reason: pi_ai::StopReason::ToolUse,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: 0,
				duration_ms: None,
			})),
		};
		let mut result = entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::ToolResult(pi_ai::ToolResultMessage {
				tool_call_id: "call-1".into(),
				tool_name: "edit".into(),
				content: vec![pi_ai::TextOrImage::text("ok")],
				details: Some(json!({"diff": "+added\n-removed\n context"})),
				usage: None,
				is_error: false,
				timestamp: 0,
				duration_ms: None,
			})),
		});
		result.base.id = Some("r1".into());
		result.base.parent_id = Some("a1".into());
		let html = render_session_html(None, &[assistant, result], Some("r1"));
		assert!(html.contains("<div class=\"tool-header\"><span class=\"tool-name\">edit</span> <span class=\"tool-path\">/tmp/x.rs</span></div>"));
		assert!(html.contains("<div class=\"diff-added\">+added</div>"));
		assert!(html.contains("<div class=\"diff-removed\">-removed</div>"));
		assert!(html.contains("<div class=\"diff-context\"> context</div>"));
	}

	#[test]
	fn unknown_tools_render_the_json_args_fallback() {
		let (mut assistant, mut call) = assistant_with_call();
		call.name = "mcp__srv__widget".into();
		call.arguments = json!({"query": "a<b"});
		assistant.kind = EntryKind::Message {
			message: Box::new(pi_ai::Message::Assistant(pi_ai::AssistantMessage {
				content: vec![pi_ai::AssistantContent::ToolCall(call)],
				api: "mock".into(),
				provider: "mock".into(),
				model: "mock-echo".into(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: pi_ai::Usage::default(),
				stop_reason: pi_ai::StopReason::ToolUse,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: 0,
				duration_ms: None,
			})),
		};
		let html = render_session_html(None, &[assistant], Some("a1"));
		assert!(html.contains(
			"<div class=\"tool-header\"><span class=\"tool-name\">mcp__srv__widget</span></div>"
		));
		assert!(html.contains("<div class=\"tool-output\"><pre>{"));
		assert!(html.contains("&quot;query&quot;: &quot;a&lt;b"));
	}

	#[test]
	fn ansi_in_tool_output_is_converted_not_shown_raw() {
		let (assistant, _) = assistant_with_call();
		let mut result = result_entry("call-1", "\x1b[31merr\x1b[0m", false);
		result.base.id = Some("r1".into());
		result.base.parent_id = Some("a1".into());
		let html = render_session_html(None, &[assistant, result], Some("r1"));
		assert!(
			html.contains(
				"<div class=\"ansi-line\"><span style=\"color:#800000\">err</span></div>"
			)
		);
	}

	// -- Branch + misc entries ------------------------------------------------

	#[test]
	fn branch_path_renders_only_the_leaf_chain() {
		let mut off_branch = user_entry("off branch");
		off_branch.base.id = Some("u2".into());
		let mut on_branch = user_entry("on branch");
		on_branch.base.id = Some("u3".into());
		on_branch.base.parent_id = Some("u2".into());
		// Tree: u2 → u3; a sibling root is never rendered.
		let mut sibling = user_entry("sibling");
		sibling.base.id = Some("u4".into());
		let html = render_session_html(None, &[off_branch, on_branch, sibling], Some("u3"));
		assert!(html.contains("on branch"));
		assert!(html.contains("off branch"));
		assert!(!html.contains("sibling"));
	}

	#[test]
	fn model_change_and_compaction_render_their_blocks() {
		let mut change = entry(EntryKind::ModelChange {
			provider: "mock".into(),
			model_id: "mock-echo".into(),
		});
		change.base.id = Some("m1".into());
		let mut compaction = SessionEntry::compaction("the summary <here>", 12_345);
		compaction.base.id = Some("c1".into());
		compaction.base.parent_id = Some("m1".into());
		let html = render_session_html(None, &[change, compaction], Some("c1"));
		assert!(
			html.contains("Switched to model: <span class=\"model-name\">mock/mock-echo</span>")
		);
		assert!(html.contains("<div class=\"compaction-label\">[compaction]</div>"));
		assert!(html.contains("Compacted from 12345 tokens"));
		assert!(html.contains("the summary &lt;here&gt;"));
	}

	#[test]
	fn system_prompt_leading_vs_update_and_tool_lists() {
		let mut system = entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::System(pi_ai::SystemMessage {
				content: pi_ai::SystemContent::Text("You are pi.".into()),
				sections: None,
				tools_added: Some(vec![pi_ai::Tool::new("read", "Read files", json!({}))]),
				tools_removed: None,
				timestamp: 0,
			})),
		});
		system.base.id = Some("s1".into());
		let html = render_session_html(None, &[system], Some("s1"));
		assert!(html.contains("<div class=\"system-prompt-header\">System Prompt</div>"));
		assert!(html.contains("<div class=\"tools-header\">Available Tools</div>"));

		let mut update = entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::System(pi_ai::SystemMessage {
				content: pi_ai::SystemContent::Text("More rules.".into()),
				sections: None,
				tools_added: None,
				tools_removed: None,
				timestamp: 0,
			})),
		});
		update.base.id = Some("s2".into());
		update.base.parent_id = Some("s1".into());
		let mut leading = entry(EntryKind::Message {
			message: Box::new(pi_ai::Message::System(pi_ai::SystemMessage {
				content: pi_ai::SystemContent::Text("first".into()),
				sections: None,
				tools_added: None,
				tools_removed: None,
				timestamp: 0,
			})),
		});
		leading.base.id = Some("s1".into());
		let html = render_session_html(None, &[leading, update], Some("s2"));
		assert!(html.contains("<div class=\"system-prompt-header\">System Prompt Update</div>"));
	}

	// -- write_session_html ---------------------------------------------------

	#[test]
	fn write_session_html_writes_default_and_explicit_paths() {
		let dir = std::env::temp_dir().join(format!("pi-rs-export-test-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let session_dir = dir.join("sess-abc");
		std::fs::create_dir_all(&session_dir).unwrap();
		let header = "e1";
		let line = format!(
			"{}\n{}\n",
			json!({"type": "session", "version": 3, "id": "sid-1", "timestamp": "2026-01-02T03:04:05.000Z", "cwd": "/tmp"}),
			json!({"type": "message", "id": header, "parentId": null, "timestamp": "2026-01-02T03:04:05.000Z",
				"message": {"role": "user", "content": [{"type": "text", "text": "hello export"}], "timestamp": 0}})
		);
		std::fs::write(session_dir.join("session.jsonl"), line).unwrap();

		// Explicit relative path resolves against the process cwd.
		let explicit = write_session_html(&session_dir, Some(Path::new("exported.html")))
			.expect("write succeeds");
		let cwd = std::env::current_dir().unwrap();
		assert_eq!(explicit, cwd.join("exported.html"));
		let written = std::fs::read_to_string(&explicit).unwrap();
		assert!(written.contains("hello export"));
		assert!(written.contains("Session: sid-1"));
		std::fs::remove_file(&explicit).unwrap();

		// Default name: <app>-session-<session dir name>.html in the cwd.
		let default = write_session_html(&session_dir, None).expect("write succeeds");
		assert_eq!(
			default,
			cwd.join(format!("{APP_NAME}-session-sess-abc.html"))
		);
		std::fs::remove_file(&default).unwrap();

		// Missing session log mirrors the upstream error text.
		let empty = dir.join("no-log");
		std::fs::create_dir_all(&empty).unwrap();
		let error = write_session_html(&empty, None).unwrap_err();
		assert!(error.to_string().contains("Nothing to export yet"));

		std::fs::remove_dir_all(&dir).unwrap();
	}
}
