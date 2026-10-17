use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::{App, Item, LoginPrompt, ToolStatus};
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
    app.max_scroll = max_scroll;

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
                let mut rendered = crate::markdown::lines(text, width);
                if rendered.is_empty() {
                    rendered.push(Line::default());
                }
                if *streaming {
                    if let Some(last) = rendered.last_mut() {
                        last.spans
                            .push(Span::styled("▌", Style::new().fg(Color::DarkGray)));
                    }
                }
                lines.extend(rendered);
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
    let cache = match status.cache_hit_rate() {
        Some(rate) => format!("cache {rate}%"),
        None => "cache -".to_string(),
    };
    let right = format!(
        "↑{} ↓{} · {} · {}",
        crate::format_tokens(status.input_tokens),
        crate::format_tokens(status.output_tokens),
        cache,
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
    if let Some(login) = &app.login {
        render_login_input(login, frame, rect);
        return;
    }
    let hint = if app.busy {
        " running — Ctrl+C to interrupt "
    } else if !app.logged_in {
        " Not logged in · type /login for DeepSeek · Ctrl+C to quit "
    } else if app.input.text.is_empty() {
        " Enter to send · Ctrl+C to quit · wheel/PgUp/PgDn to scroll "
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

/// Masked key entry: the key itself never reaches the framebuffer.
fn render_login_input(login: &LoginPrompt, frame: &mut Frame, rect: Rect) {
    let hint = if login.busy {
        " verifying key… "
    } else {
        " paste API key · Enter to save · Esc to cancel "
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            format!(" {} login ", login.provider),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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

    let chars = login.input.text.chars().count();
    let available = (inner.width - 2) as usize;
    let start = chars.saturating_sub(available);
    let visible = "•".repeat(chars - start);
    let line = Line::from(vec![
        Span::styled(
            "❯ ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(visible),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
    frame.set_cursor_position((inner.x + 2 + (chars - start) as u16, inner.y));
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
    fn assistant_markdown_is_rendered() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::AssistantDone {
            text: "# Title\n- item\nplain `code`".into(),
        });

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let screen: String = buffer.content.iter().map(|c| c.symbol()).collect();

        assert!(screen.contains("Title"));
        assert!(!screen.contains('#'));
        assert!(screen.contains("• item"));
        assert!(screen.contains("plain code"));

        // Inline code keeps its accent color all the way to the framebuffer.
        let at = screen.find("code").expect("code visible");
        let cell = &buffer.content[at];
        assert_eq!(cell.fg, Color::Rgb(229, 192, 123));
        assert_ne!(buffer.content[at - 6].fg, Color::Rgb(229, 192, 123));
    }

    #[test]
    fn status_bar_shows_cache_hit_rate() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::StatusInfo;

        let status = StatusInfo {
            model: "deepseek-flash".into(),
            dir: "proj".into(),
            input_tokens: 10_000,
            output_tokens: 500,
            cached_tokens: 7_500,
            cost: "$0.01".into(),
            ..StatusInfo::default()
        };
        let mut app = App::new(status);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("cache 75%"),
            "status line missing: {screen}"
        );
    }

    #[test]
    fn login_prompt_masks_the_key() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.begin_login("DeepSeek");
        app.apply(UiEvent::Info("sign in".into()));
        for c in "sk-secret".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!screen.contains("sk-secret"), "key leaked: {screen}");
        assert!(screen.contains("DeepSeek login"), "missing title: {screen}");
        assert!(screen.contains("••••"), "missing mask: {screen}");
    }

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
