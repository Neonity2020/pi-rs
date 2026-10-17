use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::{App, Item, ToolStatus};
use crate::wrap::{display_width, wrap};

const INPUT_HEIGHT: u16 = 3;
const STATUS_HEIGHT: u16 = 1;
const TOOL_PREVIEW_LINES: usize = 6;

/// Draw the whole UI. Also keeps viewport/scroll bookkeeping in sync.
pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < 8 || area.height < STATUS_HEIGHT + INPUT_HEIGHT + 1 {
        return;
    }

    let transcript_height = area.height - INPUT_HEIGHT - STATUS_HEIGHT;
    let transcript_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: transcript_height,
    };
    let status_rect = Rect {
        x: area.x,
        y: area.y + transcript_height,
        width: area.width,
        height: STATUS_HEIGHT,
    };
    let input_rect = Rect {
        x: area.x,
        y: area.y + transcript_height + STATUS_HEIGHT,
        width: area.width,
        height: INPUT_HEIGHT,
    };

    render_transcript(app, frame, transcript_rect);
    render_status(app, frame, status_rect);
    render_input(app, frame, input_rect);
}

fn render_transcript(app: &mut App, frame: &mut Frame, rect: Rect) {
    let lines = transcript_lines(app, rect.width as usize);
    let max_scroll = lines.len().saturating_sub(rect.height as usize);
    let scroll = if app.follow {
        max_scroll
    } else {
        app.scroll.min(max_scroll)
    };
    app.scroll = scroll;
    app.viewport = rect.height;

    let paragraph =
        Paragraph::new(Text::from(lines)).scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, rect);
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    for item in &app.items {
        match item {
            Item::User(text) => {
                for (i, segment) in wrap(text, width.saturating_sub(2)).into_iter().enumerate() {
                    let prefix = if i == 0 { "❯ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(
                            prefix.to_string(),
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(segment),
                    ]));
                }
                lines.push(Line::default());
            }
            Item::Assistant {
                text,
                reasoning,
                streaming,
            } => {
                if !reasoning.is_empty() {
                    for segment in wrap(reasoning, width.saturating_sub(2)) {
                        lines.push(Line::from(Span::styled(
                            format!("  {segment}"),
                            Style::new()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
                let wrapped = wrap(text, width);
                let last = wrapped.len().saturating_sub(1);
                for (i, segment) in wrapped.into_iter().enumerate() {
                    let mut spans = vec![Span::raw(segment)];
                    if *streaming && i == last {
                        spans.push(Span::styled("▌", Style::new().fg(Color::DarkGray)));
                    }
                    lines.push(Line::from(spans));
                }
                if !*streaming && (!text.is_empty() || !reasoning.is_empty()) {
                    lines.push(Line::default());
                }
            }
            Item::Tool {
                name,
                summary,
                status,
                output,
                duration_ms,
                ..
            } => {
                let (mark, mark_style, timing) = match status {
                    ToolStatus::Running => (
                        app.spinner_char().to_string(),
                        Style::new().fg(Color::Yellow),
                        String::new(),
                    ),
                    ToolStatus::Ok => (
                        "✓".to_string(),
                        Style::new().fg(Color::Green),
                        format!(" {duration_ms}ms"),
                    ),
                    ToolStatus::Failed => (
                        "✗".to_string(),
                        Style::new().fg(Color::Red),
                        format!(" {duration_ms}ms"),
                    ),
                };
                lines.push(Line::from(vec![
                    Span::styled("⏺ ", Style::new().fg(Color::Magenta)),
                    Span::raw(format!("{name}({summary})")),
                    Span::raw(" "),
                    Span::styled(format!("{mark}{timing}"), mark_style),
                ]));

                if !output.is_empty() && *status != ToolStatus::Running {
                    let preview_style = match status {
                        ToolStatus::Failed => {
                            Style::new().fg(Color::Red).add_modifier(Modifier::DIM)
                        }
                        _ => Style::new().fg(Color::DarkGray),
                    };
                    let all: Vec<&str> = output.lines().collect();
                    let shown = all.len().min(TOOL_PREVIEW_LINES);
                    for segment in all[..shown]
                        .iter()
                        .flat_map(|l| wrap(l, width.saturating_sub(2)))
                    {
                        lines.push(Line::from(Span::styled(
                            format!("  {segment}"),
                            preview_style,
                        )));
                    }
                    if all.len() > shown {
                        lines.push(Line::from(Span::styled(
                            format!("  … +{} lines", all.len() - shown),
                            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                        )));
                    }
                    lines.push(Line::default());
                }
            }
            Item::Info(text) => {
                for segment in wrap(text, width) {
                    lines.push(Line::from(Span::styled(
                        segment,
                        Style::new().fg(Color::DarkGray),
                    )));
                }
            }
            Item::Error(text) => {
                for segment in wrap(text, width) {
                    lines.push(Line::from(Span::styled(
                        segment,
                        Style::new().fg(Color::Red),
                    )));
                }
            }
        }
    }
    lines
}

fn render_status(app: &App, frame: &mut Frame, rect: Rect) {
    let status = &app.status;
    let left = format!(
        "{} pi-rs · {} · {}",
        app.spinner_char(),
        status.model,
        status.dir
    );
    let right = format!(
        "↑{} ↓{} · cached {} · {}",
        crate::format_tokens(status.input_tokens),
        crate::format_tokens(status.output_tokens),
        crate::format_tokens(status.cached_tokens),
        status.cost
    );

    let total = rect.width as usize;
    let right_width = display_width(&right).min(total);
    let left = clip(&left, total.saturating_sub(right_width + 2));
    let padding = total
        .saturating_sub(display_width(&left) + right_width)
        .max(1);

    let line = Line::from(vec![
        Span::styled(left, Style::new().fg(Color::DarkGray)),
        Span::raw(" ".repeat(padding)),
        Span::styled(right, Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

fn render_input(app: &App, frame: &mut Frame, rect: Rect) {
    let hint = if app.busy {
        " running — Ctrl+C to interrupt "
    } else if app.input.text.is_empty() {
        " Enter to send · Ctrl+C to quit · PgUp/PgDn to scroll "
    } else {
        " Enter to send "
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Line::from(Span::styled(
            " pi ",
            Style::new().fg(Color::DarkGray),
        )))
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width < 4 {
        return;
    }

    let display = app.input.text.replace('\t', "    ");
    let before: String = display.chars().take(app.input.cursor).collect();
    let cursor_col = display_width(&before);
    let available = (inner.width - 2) as usize;
    let h_scroll = if available > 0 && cursor_col >= available {
        cursor_col - available + 1
    } else {
        0
    };
    let visible = slice_columns(&display, h_scroll, available);

    let line = Line::from(vec![
        Span::styled(
            "❯ ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(visible),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
    frame.set_cursor_position((inner.x + 2 + (cursor_col - h_scroll) as u16, inner.y));
}

/// Take `max_cols` display columns from `s`, starting at column `start`.
fn slice_columns(s: &str, start: usize, max_cols: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    let mut taken = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if col < start {
            col += w;
            continue;
        }
        if taken + w > max_cols {
            break;
        }
        out.push(c);
        taken += w;
    }
    out
}

/// Truncate to `max_cols` display columns, adding `…` when clipped.
fn clip(s: &str, max_cols: usize) -> String {
    if display_width(s) <= max_cols {
        return s.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if display_width(&out) + w > max_cols.saturating_sub(1) {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello", 4), "hel…");
        assert_eq!(clip("你好世界", 5), "你好…");
    }

    #[test]
    fn column_slicing() {
        assert_eq!(slice_columns("hello", 0, 3), "hel");
        assert_eq!(slice_columns("hello", 2, 3), "llo");
        assert_eq!(slice_columns("hello", 3, 10), "lo");
    }
}
