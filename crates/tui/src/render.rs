use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::{App, Item, LoginPrompt, ThinkingHit, ToolEntry, ToolStatus};
use crate::theme;
use crate::wrap::{display_width, wrap};

const INPUT_HEIGHT: u16 = 3;
const STATUS_HEIGHT: u16 = 1;
const TOOL_PREVIEW_LINES: usize = 6;
const CONTENT_MAX_WIDTH: u16 = 120;

/// Draw the whole UI. Also keeps viewport/scroll bookkeeping in sync.
pub fn render(app: &mut App, frame: &mut Frame) {
    let area = content_area(frame.area());
    app.thinking_hits.clear();
    app.transcript_top = area.y;
    let input_height = INPUT_HEIGHT + u16::from(!app.images.is_empty());
    if area.width < 8 || area.height < STATUS_HEIGHT + input_height + 1 {
        return;
    }

    let transcript_height = area.height - input_height - STATUS_HEIGHT;
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
        height: input_height,
    };

    render_transcript(app, frame, transcript_rect);
    render_completion(app, frame, transcript_rect);
    render_status(app, frame, status_rect);
    render_input(app, frame, input_rect);
}

/// Keep long lines readable on large terminals while leaving compact layouts
/// untouched. This mirrors the calm, centered conversation column used by
/// modern coding TUIs without adding another pane or navigation concept.
fn content_area(area: Rect) -> Rect {
    let width = area.width.min(CONTENT_MAX_WIDTH);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y,
        width,
        height: area.height,
    }
}

fn render_transcript(app: &mut App, frame: &mut Frame, rect: Rect) {
    let (lines, hits) = transcript_lines(app, rect.width as usize);
    app.thinking_hits = hits;
    app.transcript_top = rect.y;
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

fn thinking_header(
    streaming: bool,
    expanded: bool,
    reasoning: &str,
    tools: &[ToolEntry],
    spinner: &str,
) -> Line<'static> {
    let style = Style::new().fg(theme::MUTED);
    let mut title = if streaming {
        format!("{spinner} 正在思考…")
    } else if expanded {
        "▾ 思考结果".to_string()
    } else {
        "▸ 思考结果".to_string()
    };
    let mut details = Vec::new();
    let reasoning_lines = reasoning.lines().count();
    if reasoning_lines > 0 {
        details.push(format!("{reasoning_lines} 行推理"));
    }
    if !tools.is_empty() {
        details.push(format!("{} 个工具", tools.len()));
    }
    let failed = tools
        .iter()
        .filter(|tool| tool.status == ToolStatus::Failed)
        .count();
    if failed > 0 {
        details.push(format!("{failed} 失败"));
    }
    if !details.is_empty() {
        title.push_str(" · ");
        title.push_str(&details.join(" · "));
    }
    if !expanded {
        title.push_str(" · 点击展开");
    }
    Line::from(Span::styled(title, style))
}

fn tool_lines(tool: &ToolEntry, width: usize, spinner: &str) -> Vec<Line<'static>> {
    let (mark, mark_style, timing) = match tool.status {
        ToolStatus::Running => (
            spinner.to_string(),
            Style::new().fg(theme::WARNING),
            String::new(),
        ),
        ToolStatus::Ok => (
            "✓".to_string(),
            Style::new().fg(theme::SUCCESS),
            format!(" {}ms", tool.duration_ms),
        ),
        ToolStatus::Failed => (
            "✗".to_string(),
            Style::new().fg(theme::ERROR),
            format!(" {}ms", tool.duration_ms),
        ),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("  ├─ ", Style::new().fg(theme::BORDER)),
        Span::styled(tool.name.clone(), Style::new().fg(theme::SECONDARY)),
        Span::styled(format!("({})", tool.summary), Style::new().fg(theme::MUTED)),
        Span::raw(" "),
        Span::styled(format!("{mark}{timing}"), mark_style),
    ])];

    if !tool.output.is_empty() && tool.status != ToolStatus::Running {
        let preview_style = match tool.status {
            ToolStatus::Failed => Style::new().fg(theme::ERROR).add_modifier(Modifier::DIM),
            _ => Style::new().fg(theme::MUTED),
        };
        let all: Vec<&str> = tool.output.lines().collect();
        let shown = all.len().min(TOOL_PREVIEW_LINES);
        for segment in all[..shown]
            .iter()
            .flat_map(|line| wrap(line, width.saturating_sub(4).max(1)))
        {
            lines.push(Line::from(Span::styled(
                format!("    {segment}"),
                preview_style,
            )));
        }
        if all.len() > shown {
            lines.push(Line::from(Span::styled(
                format!("    … +{} lines", all.len() - shown),
                Style::new().fg(theme::MUTED).add_modifier(Modifier::DIM),
            )));
        }
    }
    lines
}

fn transcript_lines(app: &App, width: usize) -> (Vec<Line<'static>>, Vec<ThinkingHit>) {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<ThinkingHit> = Vec::new();

    for item in &app.items {
        match item {
            Item::User { text, images } => {
                if text.is_empty() {
                    if !images.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "┃ ",
                                Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled("[image]", Style::new().fg(theme::MUTED)),
                        ]));
                    }
                } else {
                    for segment in wrap(text, width.saturating_sub(2)) {
                        lines.push(Line::from(vec![
                            Span::styled(
                                "┃ ",
                                Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(segment),
                        ]));
                    }
                }
                for (i, label) in images.iter().enumerate() {
                    lines.push(Line::from(vec![
                        Span::styled("┃ ", Style::new().fg(theme::PRIMARY)),
                        Span::styled("▣ ", Style::new().fg(theme::PURPLE)),
                        Span::styled(format!("[{}] ", i + 1), Style::new().fg(theme::PRIMARY)),
                        Span::styled(label.clone(), Style::new().fg(theme::MUTED)),
                    ]));
                }
                lines.push(Line::default());
            }
            Item::Thinking {
                id,
                reasoning,
                tools,
                streaming,
            } => {
                let expanded = app.thinking_expanded(*id);
                hits.push(ThinkingHit {
                    line: lines.len(),
                    id: *id,
                });
                lines.push(thinking_header(
                    *streaming,
                    expanded,
                    reasoning,
                    tools,
                    app.spinner_char(),
                ));
                if expanded {
                    let reasoning_style =
                        Style::new().fg(theme::MUTED).add_modifier(Modifier::ITALIC);
                    if !reasoning.is_empty() {
                        for segment in wrap(reasoning, width.saturating_sub(2)) {
                            lines.push(Line::from(Span::styled(
                                format!("  {segment}"),
                                reasoning_style,
                            )));
                        }
                    }
                    for tool in tools {
                        lines.extend(tool_lines(tool, width, app.spinner_char()));
                    }
                }
                if !*streaming {
                    lines.push(Line::default());
                }
            }
            Item::Assistant { text, streaming } => {
                let mut rendered = crate::markdown::lines(text, width);
                if rendered.is_empty() && !text.is_empty() {
                    rendered.push(Line::default());
                }
                if *streaming {
                    let cursor = Span::styled("▌", Style::new().fg(theme::PRIMARY));
                    if let Some(last) = rendered.last_mut() {
                        last.spans.push(cursor);
                    } else {
                        rendered.push(Line::from(cursor));
                    }
                }
                lines.extend(rendered);
                if !*streaming && !text.is_empty() {
                    lines.push(Line::default());
                }
            }
            Item::Info(text) => {
                for segment in wrap(text, width.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled("· ", Style::new().fg(theme::SECONDARY)),
                        Span::styled(segment, Style::new().fg(theme::MUTED)),
                    ]));
                }
            }
            Item::Error(text) => {
                for segment in wrap(text, width.saturating_sub(2)) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "× ",
                            Style::new().fg(theme::ERROR).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(segment, Style::new().fg(theme::ERROR)),
                    ]));
                }
            }
        }
    }
    (lines, hits)
}

fn render_status(app: &App, frame: &mut Frame, rect: Rect) {
    let status = &app.status;
    let lab = if app.lab { " · LAB" } else { "" };
    let left = format!(
        "{} pi-rs{} · {} · {}",
        app.spinner_char(),
        lab,
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
        Span::styled(left, Style::new().fg(theme::MUTED)),
        Span::raw(" ".repeat(padding)),
        Span::styled(right, Style::new().fg(theme::MUTED)),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
}

/// Slash-command menu, drawn over the bottom of the transcript so it sits
/// right above the input box. Opens while the input is a bare `/` prefix.
fn render_completion(app: &App, frame: &mut Frame, rect: Rect) {
    let Some(completion) = &app.completion else {
        return;
    };
    const MAX_ROWS: usize = 8;
    let visible = completion.matches.len().min(MAX_ROWS);
    let height = (visible + 2) as u16;
    if visible == 0 || rect.height < height || rect.width < 16 {
        return;
    }

    let usage_width = completion
        .matches
        .iter()
        .map(|command| display_width(&command.usage()))
        .max()
        .unwrap_or(0);
    let help_width = completion
        .matches
        .iter()
        .map(|command| display_width(command.help) + if command.lab { 6 } else { 0 })
        .max()
        .unwrap_or(0);
    // marker + usage + gap + help + borders
    let width = (usage_width + help_width + 6)
        .min(rect.width as usize)
        .max(16) as u16;
    let area = Rect {
        x: rect.x,
        y: rect.y + rect.height - height,
        width,
        height,
    };

    let start = completion.selected.saturating_sub(visible - 1);
    let mut lines = Vec::with_capacity(visible);
    for (row, command) in completion
        .matches
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
        let selected = row == completion.selected;
        let marker = if selected { "▸ " } else { "  " };
        let usage = format!("{:<width$}", command.usage(), width = usage_width);
        let name_style = if selected {
            Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme::TEXT)
        };
        let mut spans = vec![
            Span::styled(marker, Style::new().fg(theme::PRIMARY)),
            Span::styled(usage, name_style),
            Span::raw("  "),
            Span::styled(command.help, Style::new().fg(theme::MUTED)),
        ];
        if command.lab {
            spans.push(Span::styled(
                " · lab",
                Style::new().fg(theme::PURPLE).add_modifier(Modifier::DIM),
            ));
        }
        let mut line = Line::from(spans);
        if selected {
            line = line.style(Style::new().bg(theme::SELECTED));
        }
        lines.push(line);
    }

    let block = Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(Style::new().fg(theme::PRIMARY))
        .title(Line::from(Span::styled(
            " commands · Tab complete · ↑↓ select · Esc close ",
            Style::new().fg(theme::MUTED).add_modifier(Modifier::DIM),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_input(app: &App, frame: &mut Frame, rect: Rect) {
    if let Some(login) = &app.login {
        render_login_input(login, frame, rect);
        return;
    }
    let hint = if app.busy {
        " running — Ctrl+C to interrupt ".to_string()
    } else if !app.logged_in {
        " Not logged in · type /login for DeepSeek · Ctrl+C to quit ".to_string()
    } else if !app.images.is_empty() {
        let plural = if app.images.len() == 1 { "" } else { "s" };
        format!(
            " Enter to send · {} image{plural} · Ctrl+V add · Esc remove image ",
            app.images.len()
        )
    } else if app.input.text.is_empty() {
        " Enter to send · Ctrl+V image · Ctrl+T thinking · Ctrl+C quit · wheel to scroll "
            .to_string()
    } else {
        " Enter to send · Ctrl+V image ".to_string()
    };
    let border_color = if app.busy {
        theme::WARNING
    } else if app.logged_in {
        theme::PRIMARY
    } else {
        theme::BORDER
    };
    let block = Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(Style::new().fg(border_color))
        .title(Line::from(Span::styled(
            " pi-rs ",
            Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
        )))
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::new().fg(theme::MUTED).add_modifier(Modifier::DIM),
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
            Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
        ),
        Span::raw(visible),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
    frame.set_cursor_position((inner.x + 2 + (cursor_col - h_scroll) as u16, inner.y));

    if !app.images.is_empty() && inner.height >= 2 {
        let mut spans = Vec::new();
        for (i, image) in app.images.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("   "));
            }
            spans.push(Span::styled(
                format!("[{}]", i + 1),
                Style::new().fg(theme::PRIMARY),
            ));
            spans.push(Span::styled(
                format!(" {}", image.label()),
                Style::new().fg(theme::MUTED),
            ));
        }
        let attachments = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(spans)), attachments);
    }
}

/// Masked key entry: the key itself never reaches the framebuffer.
fn render_login_input(login: &LoginPrompt, frame: &mut Frame, rect: Rect) {
    let hint = if login.busy {
        " verifying key… "
    } else {
        " paste API key · Enter to save · Esc to cancel "
    };
    let block = Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(Style::new().fg(theme::PRIMARY))
        .title(Line::from(Span::styled(
            format!(" {} login ", login.provider),
            Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
        )))
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::new().fg(theme::MUTED).add_modifier(Modifier::DIM),
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
            Style::new().fg(theme::PRIMARY).add_modifier(Modifier::BOLD),
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
        assert_eq!(cell.fg, theme::WARNING);
        assert_ne!(buffer.content[at - 6].fg, theme::WARNING);
    }

    #[test]
    fn thinking_is_collapsed_until_ctrl_t() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("secret plan\nsecond line".into()));
        app.apply(UiEvent::AssistantDelta("answer".into()));
        app.apply(UiEvent::AssistantDone {
            text: "answer".into(),
        });

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        let flat = screen.replace(' ', "");
        assert!(
            flat.contains("▸思考结果·2行推理·点击展开"),
            "missing header: {screen}"
        );
        assert!(screen.contains("answer"));
        assert!(
            !screen.contains("secret plan"),
            "reasoning leaked: {screen}"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        let flat = screen.replace(' ', "");
        assert!(
            flat.contains("▾思考结果"),
            "missing expanded header: {screen}"
        );
        assert!(
            screen.contains("secret plan"),
            "reasoning not expanded: {screen}"
        );
    }

    #[test]
    fn streaming_thinking_shows_a_live_header() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("still thinking".into()));

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        let flat = screen.replace(' ', "");
        assert!(flat.contains("正在思考…"), "missing live header: {screen}");
        assert!(
            !screen.contains("still thinking"),
            "reasoning leaked: {screen}"
        );
    }

    #[test]
    fn finished_thinking_is_labeled_as_a_result() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("secret plan".into()));
        app.apply(UiEvent::AssistantDelta("answer".into()));

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        let flat = screen.replace(' ', "");
        assert!(
            flat.contains("▸思考结果·1行推理·点击展开"),
            "missing finished header: {screen}"
        );
        assert!(
            !flat.contains("正在思考"),
            "live label outlived reasoning: {screen}"
        );
        assert!(screen.contains("answer"));
    }

    #[test]
    fn tool_calls_are_folded_and_click_expands_the_block() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("secret plan".into()));
        app.apply(UiEvent::ToolStart {
            id: "c1".into(),
            name: "read".into(),
            summary: "src/lib.rs".into(),
        });
        app.apply(UiEvent::ToolDone {
            id: "c1".into(),
            ok: true,
            output: "file body".into(),
            duration_ms: 4,
        });
        app.apply(UiEvent::AssistantDone {
            text: "answer".into(),
        });

        let width = 60;
        let mut terminal = Terminal::new(TestBackend::new(width, 14)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        let flat = screen.replace(' ', "");
        assert!(flat.contains("思考结果·1行推理·1个工具"), "{screen}");
        assert!(screen.contains("answer"));
        assert!(
            !screen.contains("secret plan"),
            "reasoning leaked: {screen}"
        );
        assert!(
            !screen.contains("read(src/lib.rs)"),
            "tool leaked: {screen}"
        );

        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let header = rows
            .iter()
            .position(|row| row.contains("思"))
            .expect("thinking header visible");
        assert!(app.on_click(0, header as u16));

        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("secret plan"),
            "reasoning not expanded: {screen}"
        );
        assert!(
            screen.contains("read(src/lib.rs)"),
            "tool not expanded: {screen}"
        );
        assert!(
            screen.contains("file body"),
            "tool output missing: {screen}"
        );
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
    fn lab_mode_is_visible_in_the_status_bar() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::StatusInfo;

        let mut app = App::new(StatusInfo::default());
        app.lab = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("pi-rs · LAB"),
            "missing lab marker: {screen}"
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
    fn attachments_are_visible_in_the_input() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::{ImageAttachment, StatusInfo, UiEvent};

        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ImageAttached(ImageAttachment {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            bytes: 1536,
            width: Some(640),
            height: Some(480),
        }));

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("[1]"), "missing attachment index: {screen}");
        assert!(screen.contains("PNG 640×480"), "missing label: {screen}");
        assert!(
            screen.contains("Esc remove image"),
            "missing image hint: {screen}"
        );
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

    #[test]
    fn wide_terminals_center_a_readable_conversation_column() {
        assert_eq!(
            content_area(Rect::new(0, 0, 160, 40)),
            Rect::new(20, 0, CONTENT_MAX_WIDTH, 40)
        );
        assert_eq!(
            content_area(Rect::new(3, 2, 80, 20)),
            Rect::new(3, 2, 80, 20)
        );
    }

    #[test]
    fn command_menu_is_drawn_above_the_input_box() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        use crate::app::StatusInfo;

        let mut app = App::new(StatusInfo::default());
        for c in "/lo".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();

        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(70)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let menu = rows
            .iter()
            .position(|row| row.contains("commands"))
            .expect("menu border title");
        assert!(
            rows[menu].contains("Tab complete"),
            "hint missing: {}",
            rows[menu]
        );
        assert!(rows[menu + 1].contains("▸ /login"), "{}", rows[menu + 1]);
        assert!(rows[menu + 2].contains("/logout"), "{}", rows[menu + 2]);
        // The input box stays at the bottom, below the menu.
        assert!(
            rows.iter().any(|row| row.contains("pi-rs")),
            "input title missing"
        );
    }
}
