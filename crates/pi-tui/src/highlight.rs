//! Syntax highlighting for markdown code blocks using `syntect`.
//!
//! Maps Sublime Text syntax scopes onto `pi-tui` [`Theme`] syntax colors:
//! `syntax_keyword`, `syntax_function`, `syntax_type`, `syntax_string`,
//! `syntax_number`, `syntax_comment`, `syntax_variable`, `syntax_operator`,
//! `syntax_punctuation`. Unrecognized languages gracefully return `None`
//! so callers fall back to standard `theme.md_code_block`.

use std::str::FromStr;
use std::sync::LazyLock;

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{
	Color as SynColor, ScopeSelectors, StyleModifier, Theme as SynTheme, ThemeItem, ThemeSettings,
};
use syntect::parsing::SyntaxSet;

use crate::theme::Theme;

/// Global compiled Sublime syntax definitions (bundled in binary).
pub static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Highlight lines of code with the given language tag.
/// Returns `None` if `lang` is empty or no syntax matches.
pub fn highlight_code(
	code: &[String],
	lang: &str,
	theme: &Theme,
) -> Option<Vec<Vec<Span<'static>>>> {
	let lang = lang.trim();
	if lang.is_empty() {
		return None;
	}

	let syntax = SYNTAX_SET
		.find_syntax_by_token(lang)
		.or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))?;

	let syn_theme = build_syntect_theme(theme);
	let mut highlighter = HighlightLines::new(syntax, &syn_theme);
	let mut result = Vec::with_capacity(code.len());

	for line in code {
		let line_with_nl = format!("{line}\n");
		let ranges = highlighter
			.highlight_line(&line_with_nl, &SYNTAX_SET)
			.ok()?;
		let mut spans = Vec::new();
		for (style, text) in ranges {
			let trimmed = text.strip_suffix('\n').unwrap_or(text);
			if trimmed.is_empty() {
				continue;
			}
			let color = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
			spans.push(Span::styled(trimmed.to_string(), Style::new().fg(color)));
		}
		result.push(spans);
	}

	Some(result)
}

fn to_syn_color(color: Color) -> SynColor {
	match color {
		Color::Rgb(r, g, b) => SynColor { r, g, b, a: 255 },
		Color::Black => SynColor {
			r: 0,
			g: 0,
			b: 0,
			a: 255,
		},
		Color::Red => SynColor {
			r: 205,
			g: 49,
			b: 49,
			a: 255,
		},
		Color::Green => SynColor {
			r: 13,
			g: 188,
			b: 121,
			a: 255,
		},
		Color::Yellow => SynColor {
			r: 229,
			g: 229,
			b: 16,
			a: 255,
		},
		Color::Blue => SynColor {
			r: 36,
			g: 114,
			b: 200,
			a: 255,
		},
		Color::Magenta => SynColor {
			r: 188,
			g: 63,
			b: 188,
			a: 255,
		},
		Color::Cyan => SynColor {
			r: 17,
			g: 168,
			b: 205,
			a: 255,
		},
		Color::Gray => SynColor {
			r: 150,
			g: 150,
			b: 150,
			a: 255,
		},
		Color::DarkGray => SynColor {
			r: 100,
			g: 100,
			b: 100,
			a: 255,
		},
		Color::LightRed => SynColor {
			r: 241,
			g: 76,
			b: 76,
			a: 255,
		},
		Color::LightGreen => SynColor {
			r: 35,
			g: 209,
			b: 139,
			a: 255,
		},
		Color::LightYellow => SynColor {
			r: 245,
			g: 245,
			b: 67,
			a: 255,
		},
		Color::LightBlue => SynColor {
			r: 59,
			g: 142,
			b: 234,
			a: 255,
		},
		Color::LightMagenta => SynColor {
			r: 214,
			g: 112,
			b: 214,
			a: 255,
		},
		Color::LightCyan => SynColor {
			r: 41,
			g: 184,
			b: 219,
			a: 255,
		},
		Color::White => SynColor {
			r: 229,
			g: 229,
			b: 229,
			a: 255,
		},
		Color::Indexed(idx) => SynColor {
			r: idx,
			g: idx,
			b: idx,
			a: 255,
		},
		Color::Reset => SynColor {
			r: 200,
			g: 200,
			b: 200,
			a: 255,
		},
	}
}

fn item(scope: &str, color: Color) -> Option<ThemeItem> {
	let selectors = ScopeSelectors::from_str(scope).ok()?;
	Some(ThemeItem {
		scope: selectors,
		style: StyleModifier {
			foreground: Some(to_syn_color(color)),
			background: None,
			font_style: None,
		},
	})
}

fn build_syntect_theme(theme: &Theme) -> SynTheme {
	let settings = ThemeSettings {
		foreground: Some(to_syn_color(theme.md_code_block)),
		..Default::default()
	};

	let scopes = [
		("comment", theme.syntax_comment),
		("string", theme.syntax_string),
		("constant.numeric", theme.syntax_number),
		("constant.language", theme.syntax_number),
		("keyword.control", theme.syntax_keyword),
		("keyword.operator", theme.syntax_operator),
		("keyword", theme.syntax_keyword),
		("storage", theme.syntax_keyword),
		("entity.name.function", theme.syntax_function),
		("support.function", theme.syntax_function),
		("entity.name.type", theme.syntax_type),
		("entity.name.class", theme.syntax_type),
		("entity.name.struct", theme.syntax_type),
		("entity.name.enum", theme.syntax_type),
		("support.type", theme.syntax_type),
		("support.class", theme.syntax_type),
		("variable", theme.syntax_variable),
		("punctuation", theme.syntax_punctuation),
	];

	let items = scopes.into_iter().filter_map(|(s, c)| item(s, c)).collect();

	SynTheme {
		name: None,
		author: None,
		settings,
		scopes: items,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_highlight_rust() {
		let theme = Theme::dark();
		let code = vec![
			"fn main() {".to_string(),
			"    // comment".to_string(),
			"    let x = 42;".to_string(),
			"}".to_string(),
		];
		let highlighted = highlight_code(&code, "rs", &theme).expect("rust syntax should match");
		assert_eq!(highlighted.len(), 4);
		// Check that line 0 has fn as keyword
		let line0_text: String = highlighted[0].iter().map(|s| s.content.as_ref()).collect();
		assert_eq!(line0_text, "fn main() {");
		// Check that fn is styled with syntax_keyword
		let fn_span = &highlighted[0][0];
		assert_eq!(fn_span.content, "fn");
		assert_eq!(
			fn_span.style.fg,
			Some(to_ratatui_color(theme.syntax_keyword))
		);
	}

	#[test]
	fn test_unknown_language_returns_none() {
		let theme = Theme::dark();
		let code = vec!["some plain code".to_string()];
		assert!(highlight_code(&code, "unknown_lang_xyz", &theme).is_none());
		assert!(highlight_code(&code, "", &theme).is_none());
	}

	fn to_ratatui_color(color: Color) -> Color {
		let syn = to_syn_color(color);
		Color::Rgb(syn.r, syn.g, syn.b)
	}
}
