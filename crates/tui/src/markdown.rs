//! A deliberately small Markdown renderer for assistant output.
//!
//! Block level: fenced code, headings, blockquotes, lists, tables, rules.
//! Inline: `code`, **bold**, *italic*, ~~strike~~ and [links](url). Anything it
//! does not understand is emitted literally — losing text is worse than losing
//! style.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::wrap::{display_width, wrap};

/// Palette: every accent has its own hue so styles stay distinguishable.
fn inline_code() -> Style {
    Style::new().fg(Color::Rgb(229, 192, 123)) // amber
}

fn block_code() -> Style {
    Style::new().fg(Color::Rgb(171, 178, 191)) // light gray
}

fn bold() -> Style {
    Style::new()
        .fg(Color::Rgb(97, 175, 239)) // blue
        .add_modifier(Modifier::BOLD)
}

fn italic() -> Style {
    Style::new().add_modifier(Modifier::ITALIC)
}

fn strike() -> Style {
    Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::CROSSED_OUT)
}

fn heading() -> Style {
    Style::new()
        .fg(Color::Rgb(86, 182, 194)) // teal
        .add_modifier(Modifier::BOLD)
}

fn link() -> Style {
    Style::new()
        .fg(Color::Rgb(97, 175, 239))
        .add_modifier(Modifier::UNDERLINED)
}

fn bullet() -> Style {
    Style::new().fg(Color::Rgb(86, 182, 194))
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn quote() -> Style {
    Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC)
}

fn table_header() -> Style {
    heading()
}

fn border() -> Style {
    dim()
}

/// Render Markdown into styled, already-wrapped terminal lines.
pub fn lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let src: Vec<&str> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0;

    while i < src.len() {
        let line = src[i];
        let start = line.trim_start();

        if let Some((ch, n)) = fence {
            i += 1;
            if closes_fence(start, ch, n) {
                fence = None;
            } else {
                push_code_block(&mut out, line, width);
            }
            continue;
        }
        if let Some((ch, n)) = open_fence(start) {
            fence = Some((ch, n));
            i += 1;
            continue;
        }
        if let Some(table) = table_at(&src, i) {
            render_table(&mut out, &table, width);
            i = table.next;
            continue;
        }

        i += 1;

        if line.trim().is_empty() {
            out.push(Line::default());
            continue;
        }
        if is_rule(line) {
            out.push(Line::from(Span::styled("─".repeat(width), dim())));
            continue;
        }
        if let Some((level, rest)) = heading_text(line) {
            let base = if level == 1 {
                heading().add_modifier(Modifier::UNDERLINED)
            } else {
                heading()
            };
            for rendered in wrap_spans(&parse_inline(rest), width) {
                out.push(restyle(rendered, base));
            }
            continue;
        }
        if let Some((depth, rest)) = quote_text(line) {
            let indent = "│ ".repeat(depth.min(2));
            push_prefixed(
                &mut out,
                &parse_inline(rest),
                width,
                Span::styled(indent.clone(), dim()),
                " ".repeat(display_width(&indent)),
                quote(),
            );
            continue;
        }
        if let Some((indent, marker, rest)) = list_item(line) {
            let marker = format!("{}{marker}", " ".repeat(indent));
            let pad = " ".repeat(display_width(&marker));
            push_prefixed(
                &mut out,
                &parse_inline(rest),
                width,
                Span::styled(marker, bullet()),
                pad,
                Style::new(),
            );
            continue;
        }
        for rendered in wrap_spans(&parse_inline(line), width) {
            out.push(rendered);
        }
    }

    out
}

fn push_code_block(out: &mut Vec<Line<'static>>, line: &str, width: usize) {
    let indent = "  ";
    for segment in wrap(line, width.saturating_sub(indent.len())) {
        out.push(Line::from(Span::styled(
            format!("{indent}{segment}"),
            block_code(),
        )));
    }
}

/// First line gets `first`, continuation lines get `pad`; `base` styles text.
fn push_prefixed(
    out: &mut Vec<Line<'static>>,
    spans: &[(Style, String)],
    width: usize,
    first: Span<'static>,
    pad: String,
    base: Style,
) {
    let prefix_width = display_width(&pad);
    let mut rendered = wrap_spans(spans, width.saturating_sub(prefix_width));
    if rendered.is_empty() {
        rendered.push(Line::default());
    }
    for (i, mut line) in rendered.into_iter().enumerate() {
        let prefix = if i == 0 {
            first.clone()
        } else {
            Span::raw(pad.clone())
        };
        line.spans.insert(0, prefix);
        out.push(restyle(line, base));
    }
}

fn restyle(mut line: Line<'static>, base: Style) -> Line<'static> {
    if base == Style::new() {
        return line;
    }
    for span in &mut line.spans {
        span.style = span.style.patch(base);
    }
    line
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A parsed GFM-style pipe table.
struct Table {
    header: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
    /// Index of the first line after the table.
    next: usize,
}

/// Try to read a table starting at `start` (header row, then delimiter row).
fn table_at(src: &[&str], start: usize) -> Option<Table> {
    let header_line = *src.get(start)?;
    let delim_line = *src.get(start + 1)?;
    // The delimiter row must have a pipe, otherwise `text` + `---` is a
    // setext heading or a rule, not a table.
    if !header_line.contains('|') || !delim_line.contains('|') {
        return None;
    }
    // A heading, list item or quote wins over a table header.
    if starts_block(header_line.trim()) {
        return None;
    }
    let header = split_cells(header_line);
    let aligns: Vec<Align> = split_cells(delim_line)
        .iter()
        .map(|cell| align_of(cell))
        .collect::<Option<_>>()?;
    if header.is_empty() || header.len() != aligns.len() {
        return None;
    }

    let mut rows = Vec::new();
    let mut i = start + 2;
    while let Some(line) = src.get(i) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains('|') || starts_block(trimmed) {
            break;
        }
        let mut cells = split_cells(line);
        cells.resize(header.len(), String::new());
        cells.truncate(header.len());
        rows.push(cells);
        i += 1;
    }

    Some(Table {
        header,
        aligns,
        rows,
        next: i,
    })
}

/// Split a table row on unescaped pipes, dropping optional edge pipes.
fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for c in trimmed.chars() {
        if escaped {
            cur.push('\\');
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '|' => cells.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if escaped {
        cur.push('\\');
    }
    cells.push(cur);

    if trimmed.starts_with('|') && cells.len() > 1 && cells[0].trim().is_empty() {
        cells.remove(0);
    }
    if trimmed.ends_with('|')
        && !trimmed.ends_with("\\|")
        && cells.len() > 1
        && cells.last().is_some_and(|cell| cell.trim().is_empty())
    {
        cells.pop();
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// `---`, `:--`, `--:` or `:-:`; anything else is not a delimiter cell.
fn align_of(cell: &str) -> Option<Align> {
    let cell = cell.trim();
    let (left, rest) = match cell.strip_prefix(':') {
        Some(rest) => (true, rest),
        None => (false, cell),
    };
    let (right, dashes) = match rest.strip_suffix(':') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    })
}

fn starts_block(line: &str) -> bool {
    is_rule(line)
        || heading_text(line).is_some()
        || quote_text(line).is_some()
        || list_item(line).is_some()
        || open_fence(line).is_some()
}

fn render_table(out: &mut Vec<Line<'static>>, table: &Table, width: usize) {
    let columns = table.header.len();
    let mut widths: Vec<usize> = (0..columns)
        .map(|column| {
            std::iter::once(&table.header[column])
                .chain(table.rows.iter().map(|row| &row[column]))
                .map(|cell| cell_width(cell))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();

    // 1 + Σ(w + 2) + (n - 1) columns are taken by borders and cell padding.
    let available = width.saturating_sub(3 * columns + 1);
    let mut total: usize = widths.iter().sum();
    while total > available {
        let Some(widest) = (0..columns)
            .filter(|&column| widths[column] > 1)
            .max_by_key(|&column| (widths[column], std::cmp::Reverse(column)))
        else {
            break;
        };
        widths[widest] -= 1;
        total -= 1;
    }
    if total > available {
        // Too narrow for even one column of content: degrade to plain text.
        let rows = std::iter::once(&table.header).chain(table.rows.iter());
        for row in rows {
            let text = row.join(" | ");
            for rendered in wrap_spans(&parse_inline(&text), width) {
                out.push(rendered);
            }
        }
        return;
    }

    out.push(border_line('┌', '┬', '┐', &widths));
    let header: Vec<Vec<Line<'static>>> = table
        .header
        .iter()
        .enumerate()
        .map(|(column, cell)| wrap_cell(cell, widths[column]))
        .collect();
    push_table_row(out, &header, &widths, &table.aligns, table_header());
    out.push(border_line('├', '┼', '┤', &widths));
    for row in &table.rows {
        let cells: Vec<Vec<Line<'static>>> = row
            .iter()
            .enumerate()
            .map(|(column, cell)| wrap_cell(cell, widths[column]))
            .collect();
        push_table_row(out, &cells, &widths, &table.aligns, Style::new());
    }
    out.push(border_line('└', '┴', '┘', &widths));
}

fn border_line(left: char, middle: char, right: char, widths: &[usize]) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            text.push(middle);
        }
        text.push_str(&"─".repeat(width + 2));
    }
    text.push(right);
    Line::from(Span::styled(text, border()))
}

/// Wrap one cell's inline markup to `width`, always yielding at least one line.
fn wrap_cell(cell: &str, width: usize) -> Vec<Line<'static>> {
    let mut rendered = wrap_spans(&parse_inline(cell), width);
    if rendered.is_empty() {
        rendered.push(Line::default());
    }
    rendered
}

fn push_table_row(
    out: &mut Vec<Line<'static>>,
    cells: &[Vec<Line<'static>>],
    widths: &[usize],
    aligns: &[Align],
    base: Style,
) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for index in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        push_span(&mut spans, border(), "│");
        for (column, cell) in cells.iter().enumerate() {
            let width = widths[column];
            let line = cell.get(index);
            let used = line.map(line_width).unwrap_or(0);
            let pad = width.saturating_sub(used);
            let (left, right) = match aligns[column] {
                Align::Left => (0, pad),
                Align::Right => (pad, 0),
                Align::Center => (pad / 2, pad - pad / 2),
            };
            push_span(&mut spans, Style::new(), " ");
            if left > 0 {
                push_span(&mut spans, Style::new(), &" ".repeat(left));
            }
            if let Some(line) = line {
                for span in &line.spans {
                    push_span(&mut spans, span.style.patch(base), span.content.as_ref());
                }
            }
            if right > 0 {
                push_span(&mut spans, Style::new(), &" ".repeat(right));
            }
            push_span(&mut spans, Style::new(), " ");
            push_span(&mut spans, border(), "│");
        }
        out.push(Line::from(spans));
    }
}

fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

/// Display width of a cell's text once inline markup is stripped.
fn cell_width(cell: &str) -> usize {
    parse_inline(cell)
        .iter()
        .map(|(_, text)| display_width(text))
        .sum()
}

fn open_fence(line: &str) -> Option<(char, usize)> {
    let ch = line.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let n = line.chars().take_while(|c| *c == ch).count();
    (n >= 3).then_some((ch, n))
}

fn closes_fence(line: &str, ch: char, n: usize) -> bool {
    let run = line.chars().take_while(|c| *c == ch).count();
    run >= n && line[run * ch.len_utf8()..].trim().is_empty()
}

fn heading_text(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    let rest = &line[level * '#'.len_utf8()..];
    if !(1..=6).contains(&level) || !rest.starts_with(' ') {
        return None;
    }
    Some((level, rest.trim()))
}

fn is_rule(line: &str) -> bool {
    let marks: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let mut chars = marks.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    marks.len() >= 3 && matches!(first, '-' | '*' | '_') && chars.all(|c| c == first)
}

fn quote_text(line: &str) -> Option<(usize, &str)> {
    let mut depth = 0;
    let mut rest = line.trim_start();
    while let Some(stripped) = rest.strip_prefix('>') {
        depth += 1;
        rest = stripped.strip_prefix(' ').unwrap_or(stripped);
    }
    (depth > 0).then_some((depth, rest))
}

/// `- item`, `* item`, `+ item`, `1. item`, `1) item`.
fn list_item(line: &str) -> Option<(usize, String, &str)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    let rest = &line[indent..];
    for marker in ["- ", "* ", "+ "] {
        if let Some(content) = rest.strip_prefix(marker) {
            return Some((indent, "• ".to_string(), content));
        }
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &rest[digits.len()..];
    let content = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some((indent, format!("{digits}. "), content))
}

type Styled = (Style, String);

fn parse_inline(text: &str) -> Vec<Styled> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Styled> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() && is_punctuation(chars[i + 1]) => {
                buf.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                let n = run_len(&chars, i, '`');
                if let Some(end) = find_run(&chars, i + n, '`', n) {
                    flush(&mut out, &mut buf);
                    out.push((inline_code(), code_content(&chars[i + n..end])));
                    i = end + n;
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
            '*' | '_' => {
                let n = run_len(&chars, i, c);
                if n <= 3 {
                    if let Some(end) = find_run(&chars, i + n, c, n) {
                        let content: String = chars[i + n..end].iter().collect();
                        if emphasis_ok(&content, c, &chars, i, end + n) {
                            flush(&mut out, &mut buf);
                            let style = match n {
                                1 => italic(),
                                2 => bold(),
                                _ => bold().add_modifier(Modifier::ITALIC),
                            };
                            out.push((style, content));
                            i = end + n;
                            continue;
                        }
                    }
                }
                buf.push(c);
                i += 1;
            }
            '~' => {
                let n = run_len(&chars, i, '~');
                if n == 2 {
                    if let Some(end) = find_run(&chars, i + 2, '~', 2) {
                        let content: String = chars[i + 2..end].iter().collect();
                        if !content.trim().is_empty() {
                            flush(&mut out, &mut buf);
                            out.push((strike(), content));
                            i = end + 2;
                            continue;
                        }
                    }
                }
                buf.push(c);
                i += 1;
            }
            '[' | '!' if c == '[' || chars.get(i + 1) == Some(&'[') => {
                let start = if c == '[' { i } else { i + 1 };
                if let Some((label, url, next)) = link_at(&chars, start) {
                    flush(&mut out, &mut buf);
                    if !label.is_empty() {
                        out.push((link(), label));
                    } else if !url.is_empty() {
                        out.push((link(), url));
                    }
                    i = next;
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut out, &mut buf);
    out
}

fn flush(out: &mut Vec<Styled>, buf: &mut String) {
    if !buf.is_empty() {
        out.push((Style::new(), std::mem::take(buf)));
    }
}

fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || matches!(c, '，' | '。' | '！' | '？' | '：' | '；')
}

fn run_len(chars: &[char], start: usize, ch: char) -> usize {
    chars[start..].iter().take_while(|c| **c == ch).count()
}

/// Position of the next run of exactly `n` `ch`s at or after `from`.
fn find_run(chars: &[char], from: usize, ch: char, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == ch {
            let run = run_len(chars, i, ch);
            if run == n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn code_content(chars: &[char]) -> String {
    let raw: String = chars.iter().collect();
    if raw.len() >= 2 && raw.starts_with(' ') && raw.ends_with(' ') && !raw.trim().is_empty() {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw
    }
}

fn emphasis_ok(content: &str, ch: char, chars: &[char], open: usize, close: usize) -> bool {
    if content.trim().is_empty() || content.starts_with(' ') || content.ends_with(' ') {
        return false;
    }
    if ch != '_' {
        return true;
    }
    // Intraword underscores don't create emphasis in Markdown.
    let before = open.checked_sub(1).and_then(|i| chars.get(i));
    let after = chars.get(close);
    !before.is_some_and(|c| c.is_alphanumeric()) && !after.is_some_and(|c| c.is_alphanumeric())
}

/// Parse `[label](url)` starting at the `[` index. Returns label, url, next index.
fn link_at(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let close = (start + 1..chars.len()).find(|i| chars[*i] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let url_end = (close + 2..chars.len()).find(|i| chars[*i] == ')')?;
    let label: String = chars[start + 1..close].iter().collect();
    let url: String = chars[close + 2..url_end].iter().collect();
    Some((label, url, url_end + 1))
}

/// Greedy word wrap that keeps styles attached to words.
fn wrap_spans(spans: &[Styled], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut pending_space = false;

    for (style, text) in spans {
        let expanded = text.replace('\t', "    ");
        let mut word = String::new();
        for c in expanded.chars() {
            if c.is_whitespace() {
                if !word.is_empty() {
                    push_word(
                        &mut lines,
                        &mut current,
                        &mut used,
                        width,
                        *style,
                        &word,
                        pending_space,
                    );
                    word.clear();
                }
                pending_space = true;
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            push_word(
                &mut lines,
                &mut current,
                &mut used,
                width,
                *style,
                &word,
                pending_space,
            );
            pending_space = false;
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn push_word(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    used: &mut usize,
    width: usize,
    style: Style,
    word: &str,
    space_before: bool,
) {
    let mut rest = word;
    loop {
        let w = display_width(rest);
        let sep = usize::from(space_before && *used > 0);
        if *used > 0 && *used + sep + w > width {
            lines.push(Line::from(std::mem::take(current)));
            *used = 0;
        }
        if *used > 0 && space_before {
            push_span(current, Style::new(), " ");
            *used += 1;
        }
        let room = width.saturating_sub(*used);
        if display_width(rest) <= room {
            push_span(current, style, rest);
            *used += display_width(rest);
            return;
        }
        // The word is wider than the remaining room: hard-break it.
        let mut taken = 0usize;
        let mut cut = 0usize;
        for c in rest.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if taken + cw > room && cut > 0 {
                break;
            }
            taken += cw;
            cut += c.len_utf8();
        }
        if cut == 0 {
            cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        push_span(current, style, &rest[..cut]);
        lines.push(Line::from(std::mem::take(current)));
        *used = 0;
        rest = &rest[cut..];
        if rest.is_empty() {
            return;
        }
    }
}

fn push_span(out: &mut Vec<Span<'static>>, style: Style, text: &str) {
    if let Some(last) = out.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(text);
            return;
        }
    }
    out.push(Span::styled(text.to_string(), style));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        lines(text, 200)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn first_line_with(text: &str, needle: &str) -> Line<'static> {
        lines(text, 200)
            .into_iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
            .expect("line present")
    }

    #[test]
    fn inline_code_is_colored() {
        let line = first_line_with("use `cargo` now", "cargo");
        let span = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "cargo")
            .unwrap();
        assert_eq!(span.style, inline_code());
    }

    #[test]
    fn bold_and_italic_are_styled() {
        let line = first_line_with("a **strong** b *soft* c", "strong");
        let strong = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "strong")
            .unwrap();
        assert_eq!(strong.style, bold());
        let soft = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "soft")
            .unwrap();
        assert_eq!(soft.style, italic());
    }

    #[test]
    fn punctuation_stays_attached_to_markup() {
        assert_eq!(plain("a **bold**, b `code`."), "a bold, b code.");
        assert_eq!(plain("see [docs](https://x.dev)."), "see docs.");
        assert_eq!(plain("**a**`b`*c*"), "abc");
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        assert_eq!(plain("a `b **c"), "a `b **c");
    }

    #[test]
    fn fenced_code_is_not_inline_parsed() {
        let text = "before\n```rust\nlet x = **1**;\n```\nafter";
        let rendered = lines(text, 80);
        let body = rendered
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x")))
            .unwrap();
        assert!(body.spans.iter().all(|s| s.style == block_code()));
        assert_eq!(
            body.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "  let x = **1**;"
        );
        assert!(!rendered
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style == bold())));
    }

    #[test]
    fn headings_lose_their_hashes() {
        let rendered = lines("## Title", 80);
        let text: String = rendered[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "Title");
        assert!(rendered[0]
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn bullets_preserve_indentation() {
        assert_eq!(plain("  - item"), "  • item");
        assert_eq!(plain("1. first"), "1. first");
    }

    #[test]
    fn wrapping_keeps_styles_and_width() {
        let text = "word ".repeat(20) + "`code`";
        let rendered = lines(&text, 12);
        assert!(rendered.len() > 1);
        for line in &rendered {
            let width: usize = line.spans.iter().map(|s| display_width(&s.content)).sum();
            assert!(width <= 12, "line {width} too wide");
        }
        let code = rendered
            .last()
            .unwrap()
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "code");
        assert_eq!(code.unwrap().style, inline_code());
    }

    #[test]
    fn links_render_their_label() {
        let rendered = lines("see [docs](https://example.com) here", 80);
        let text: String = rendered[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "see docs here");
        let link_span = rendered[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "docs")
            .unwrap();
        assert_eq!(link_span.style, link());
    }

    #[test]
    fn long_words_hard_break() {
        assert_eq!(plain("abcdefghij"), "abcdefghij");
        let rendered = lines("abcdefghij", 4);
        let joined: Vec<String> = rendered
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(joined, vec!["abcd", "efgh", "ij"]);
    }

    fn text_lines(text: &str, width: usize) -> Vec<String> {
        lines(text, width)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn table_renders_a_box() {
        let md = "| Name | Age |\n|------|-----|\n| Ada  | 36  |\n| Bob  | 7   |";
        assert_eq!(
            text_lines(md, 80),
            vec![
                "┌──────┬─────┐",
                "│ Name │ Age │",
                "├──────┼─────┤",
                "│ Ada  │ 36  │",
                "│ Bob  │ 7   │",
                "└──────┴─────┘",
            ]
        );
    }

    #[test]
    fn table_honours_alignment() {
        let md = "| left | center | right |\n|:-----|:------:|------:|\n| a    | b      | c     |";
        let rendered = text_lines(md, 80);
        assert_eq!(rendered[3], "│ a    │   b    │     c │");
        // The header is bold; body cells keep their alignment padding.
        assert_eq!(rendered[1], "│ left │ center │ right │");
    }

    #[test]
    fn table_wraps_cells_to_terminal_width() {
        let md = "| key | description |\n|-----|-------------|\n| alpha | a fairly long description that must wrap |";
        let rendered = text_lines(md, 30);
        for line in &rendered {
            assert!(display_width(line) <= 30, "line too wide: {line:?}");
        }
        let widths: std::collections::BTreeSet<usize> =
            rendered.iter().map(|l| display_width(l)).collect();
        assert_eq!(widths.len(), 1, "all rows share one width: {rendered:?}");
        assert!(rendered[0].starts_with('┌'));
        assert_eq!(rendered.last().unwrap().chars().next(), Some('└'));
        let joined = rendered.join("\n");
        assert!(joined.contains("alpha"));
        assert!(joined.contains("must"));
    }

    #[test]
    fn table_ends_at_blank_line() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n\n| literal | text |";
        let rendered = text_lines(md, 40);
        let table_lines = rendered
            .iter()
            .take_while(|l| l.chars().next().is_some_and(|c| "│┌├└".contains(c)))
            .count();
        assert_eq!(table_lines, 5);
        assert!(rendered.iter().any(|l| l == "| literal | text |"));
    }

    #[test]
    fn escaped_pipe_stays_inside_the_cell() {
        let md = "| a \\| b | c |\n|---|---|\n| 1 | 2 |";
        let rendered = text_lines(md, 80);
        assert_eq!(rendered[1], "│ a | b │ c │");
        assert_eq!(rendered[0].matches('┬').count(), 1);
    }

    #[test]
    fn other_blocks_win_over_table_headers() {
        let rendered = text_lines("# head | text\n|---|---|", 40);
        assert_eq!(rendered[0], "head | text");
        assert_eq!(rendered[1], "|---|---|");
    }

    #[test]
    fn pipes_without_delimiter_stay_literal() {
        assert_eq!(plain("a | b\nc | d"), "a | b\nc | d");
        assert_eq!(plain("text\n---"), format!("text\n{}", "─".repeat(200)));
    }

    #[test]
    fn table_degrades_to_text_when_too_narrow() {
        let md = "| alpha | beta |\n|---|---|\n| 1 | 2 |";
        let rendered = text_lines(md, 5);
        assert!(!rendered.iter().any(|l| l.contains('│')));
        for line in &rendered {
            assert!(display_width(line) <= 5, "line too wide: {line:?}");
        }
        assert!(rendered.join("\n").contains("alpha"));
    }

    #[test]
    fn tables_inside_fences_stay_code() {
        let md = "```\n| a | b |\n|---|---|\n```";
        let rendered = lines(md, 40);
        assert!(rendered
            .iter()
            .all(|l| l.spans.iter().all(|s| s.style == block_code())));
    }
}
