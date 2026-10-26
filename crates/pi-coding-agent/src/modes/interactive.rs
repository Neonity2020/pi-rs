//! Interactive mode — port of
//! `pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts`,
//! rendered with ratatui (pi-tui).
//!
//! Structure mirrors the TS original: `init()` builds the TUI, `run()` is the
//! `while (true) { getUserInput(); session.prompt() }` loop with agent events
//! fanned into a chat viewport. Theme system, model selector overlay,
//! tree/session pickers, and external-editor integration land later.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use pi_agent::AgentEvent;
use pi_ai::Message;
use pi_tui::components::Component;
use pi_tui::{Editor, ScrollView, Theme, TerminalGuard};

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle};
use crate::core::slash_commands::BUILTIN_SLASH_COMMANDS;

const TICK_MS: u64 = 80;

/// Structured chat state; rendered to lines every frame.
enum ChatBlock {
	User(String),
	Assistant {
		text: String,
		thinking: String,
		streaming: bool,
		error: Option<String>,
	},
	Tool {
		id: String,
		name: String,
		args: String,
		running: bool,
		ok: bool,
		preview: String,
	},
	Notice(String),
}

pub struct InteractiveMode {
	session: SessionHandle,
	model_keys: Vec<String>,
	theme: Theme,
	editor: Editor,
	chat: ScrollView,
	blocks: Vec<ChatBlock>,
	working: bool,
	quit: bool,
	tick: u64,
	cwd: String,
	usage: (u64, u64, f64), // in, out, cost
}

impl InteractiveMode {
	pub fn new(
		session: SessionHandle,
		model_keys: Vec<String>,
		cwd: String,
		theme: Theme,
	) -> Self {
		Self {
			session,
			model_keys,
			theme: theme.clone(),
			editor: Editor::new("❯ ").with_style(Style::new().fg(theme.assistant)),
			chat: ScrollView::new(),
			blocks: Vec::new(),
			working: false,
			quit: false,
			tick: 0,
			cwd,
			usage: (0, 0, 0.0),
		}
	}

	pub async fn run(mut self, initial_prompt: Option<String>) -> anyhow::Result<()> {
		let mut guard = TerminalGuard::new()?;

		// Welcome banner.
		self.push_notice(format!(
			"{} v{} — ratatui framework build. Type /help for commands; ctrl+c exits, esc aborts a run.",
			crate::config::APP_NAME,
			crate::config::VERSION
		));

		if let Some(prompt) = initial_prompt {
			if !prompt.trim().is_empty() {
				self.send_prompt(prompt);
			}
		}

		let (term_tx, mut term_rx) = mpsc::unbounded_channel::<TermEvent>();
		tokio::spawn(async move {
			let mut stream = crossterm::event::EventStream::new();
			use futures::StreamExt;
			while let Some(Ok(event)) = stream.next().await {
				if term_tx.send(event).is_err() {
					break;
				}
			}
		});

		loop {
			guard.terminal.draw(|frame| self.draw(frame))?;

			tokio::select! {
				event = term_rx.recv() => {
					match event {
						Some(event) => self.handle_terminal_event(event),
						None => self.quit = true, // input stream closed
					}
				}
				event = self.session.events.recv() => {
					match event {
						Ok(event) => self.handle_session_event(event),
						Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
							self.push_notice(format!("event lag: dropped {count} events"));
						}
						Err(tokio::sync::broadcast::error::RecvError::Closed) => {
							self.quit = true;
						}
					}
				}
				_ = tokio::time::sleep(Duration::from_millis(TICK_MS)) => {
					self.tick = self.tick.wrapping_add(1);
				}
			}

			if self.quit {
				break;
			}
		}

		let _ = self.session.cmd_tx.send(SessionCommand::Exit);
		Ok(())
	}

	// -- input handling -------------------------------------------------------

	fn handle_terminal_event(&mut self, event: TermEvent) {
		if let TermEvent::Key(key) = &event {
			if key.kind == crossterm::event::KeyEventKind::Release {
				return;
			}
			// Global keys first, mirroring pi's keybinding manager.
			if self.handle_global_key(*key) {
				return;
			}
		}
		if self.editor.handle_event(&event) {
			for editor_event in self.editor.take_events() {
				if let pi_tui::components::EditorEvent::Submit(line) = editor_event {
					self.handle_submit(line);
				}
			}
			return;
		}
		self.chat.handle_event(&event);
	}

	fn handle_global_key(&mut self, key: KeyEvent) -> bool {
		match (key.code, key.modifiers) {
			(KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
				self.quit = true;
				true
			}
			(KeyCode::Char('d'), m)
				if m.contains(KeyModifiers::CONTROL) && self.editor.is_empty() =>
			{
				self.quit = true;
				true
			}
			(KeyCode::Esc, _) => {
				if self.working {
					let _ = self.session.cmd_tx.send(SessionCommand::Abort);
				}
				true
			}
			_ => false,
		}
	}

	fn handle_submit(&mut self, line: String) {
		let line = line.trim().to_string();
		if line.is_empty() {
			return;
		}
		if let Some(command) = line.strip_prefix('/') {
			self.handle_slash(command.trim());
		} else {
			self.send_prompt(line);
		}
	}

	fn send_prompt(&mut self, text: String) {
		let _ = self.session.cmd_tx.send(SessionCommand::Prompt { text });
		self.chat.scroll_to_bottom(usize::MAX);
	}

	fn handle_slash(&mut self, command: &str) {
		let (name, args) = match command.split_once(char::is_whitespace) {
			Some((n, a)) => (n, a.trim()),
			None => (command, ""),
		};
		match name {
			"help" | "h" | "?" => {
				let mut text = String::from("commands:\n");
				for info in BUILTIN_SLASH_COMMANDS {
					text.push_str(&format!("  /{:<10} {}\n", info.name, info.description));
				}
				self.push_notice(text);
			}
			"exit" | "quit" | "q" => self.quit = true,
			"clear" => {
				self.blocks.clear();
				self.chat.clear();
			}
			"model" => {
				if args.is_empty() {
					let mut text = String::from("models:\n");
					for key in &self.model_keys {
						text.push_str(&format!("  {key}\n"));
					}
					text.push_str("\nswitch with /model <key>");
					self.push_notice(text);
				} else {
					let _ = self.session.cmd_tx.send(SessionCommand::SetModel { query: args.to_string() });
				}
			}
			"thinking" => {
				let _ = self.session.cmd_tx.send(SessionCommand::CycleThinking);
			}
			"tools" => {
				let names = self.session.agent.tool_names().join(", ");
				self.push_notice(format!("active tools: {names}"));
			}
			"prompt" => {
				let prompt = self.session.agent.snapshot().system_prompt;
				let preview: Vec<&str> = prompt.lines().take(30).collect();
				self.push_notice(format!("system prompt (first 30 lines):\n{}\n…", preview.join("\n")));
			}
			"compact" => {
				let _ = self.session.cmd_tx.send(SessionCommand::Compact);
			}
			"sessions" => {
				let sessions = crate::core::SessionManager::list_recent(10);
				if sessions.is_empty() {
					self.push_notice("no saved sessions yet".into());
				} else {
					let text = sessions
						.iter()
						.map(|(dir, entries)| {
							format!(
								"  {:<28} {} entries",
								dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
								entries
							)
						})
						.collect::<Vec<_>>()
						.join("\n");
					self.push_notice(format!("recent sessions:\n{text}"));
				}
			}
			other => {
				self.push_notice(format!("unknown command /{other} — try /help"));
			}
		}
	}

	// -- agent events -----------------------------------------------------------

	fn handle_session_event(&mut self, event: SessionEvent) {
		match event {
			SessionEvent::Agent(event) => self.handle_agent_event(event),
			SessionEvent::Notice(text) => self.push_notice(text),
			SessionEvent::Persisted { .. } => {}
		}
	}

	fn handle_agent_event(&mut self, event: AgentEvent) {
		match event {
			AgentEvent::AgentStart => {
				self.working = true;
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::AgentEnd { .. } => {
				self.working = false;
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::MessageStart { message } => {
				match &message {
					Message::User(user) => {
						self.blocks.push(ChatBlock::User(user.content.as_text()));
					}
					Message::Assistant(assistant) => {
						self.blocks.push(ChatBlock::Assistant {
							text: assistant.text(),
							thinking: assistant.thinking(),
							streaming: true,
							error: None,
						});
					}
					_ => {}
				}
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::MessageUpdate { message } => {
				if let Message::Assistant(assistant) = &message {
					if let Some(ChatBlock::Assistant { text, thinking, .. }) =
						self.blocks.iter_mut().rev().find(|b| matches!(b, ChatBlock::Assistant { streaming: true, .. }))
					{
						*text = assistant.text();
						*thinking = assistant.thinking();
					}
				}
			}
			AgentEvent::MessageEnd { message } => {
				if let Message::Assistant(assistant) = &message {
					if let Some(ChatBlock::Assistant { streaming, error, .. }) =
						self.blocks.iter_mut().rev().find(|b| matches!(b, ChatBlock::Assistant { streaming: true, .. }))
					{
						*streaming = false;
						*error = assistant.error_message.clone();
					}
					let (in_tokens, out_tokens, cost) = &mut self.usage;
					*in_tokens += assistant.usage.input;
					*out_tokens += assistant.usage.output;
					*cost += assistant.usage.cost.total.unwrap_or(0.0);
				}
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args } => {
				self.blocks.push(ChatBlock::Tool {
					id: tool_call_id,
					name: tool_name,
					args: compact_args(&args),
					running: true,
					ok: false,
					preview: String::new(),
				});
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::ToolExecutionEnd {
				tool_call_id, result, is_error, ..
			} => {
				let preview = result
					.get("content")
					.and_then(|c| c.as_array())
					.and_then(|items| items.first())
					.and_then(|item| item.as_str())
					.map(|s| first_lines(s, 2))
					.unwrap_or_default();
				if let Some(ChatBlock::Tool { running, ok, preview: p, .. }) = self
					.blocks
					.iter_mut()
					.rev()
					.find(|b| matches!(b, ChatBlock::Tool { id, running: true, .. } if *id == tool_call_id))
				{
					*running = false;
					*ok = !is_error;
					*p = preview;
				}
			}
			AgentEvent::TurnStart | AgentEvent::TurnEnd { .. } | AgentEvent::ToolExecutionUpdate { .. } => {}
		}
	}

	fn push_notice(&mut self, text: String) {
		self.blocks.push(ChatBlock::Notice(text));
		self.chat.scroll_to_bottom(usize::MAX);
	}

	// -- rendering ---------------------------------------------------------------

	fn draw(&mut self, frame: &mut Frame) {
		let area = frame.area();
		let chunks = Layout::vertical([
			Constraint::Length(1),
			Constraint::Min(3),
			Constraint::Length(3),
			Constraint::Length(1),
		])
		.split(area);

		self.draw_header(frame, chunks[0]);
		let lines = self.render_blocks();
		self.chat.set_lines(lines);
		self.chat.render(chunks[1], frame.buffer_mut());
		self.draw_editor(frame, chunks[2]);
		self.draw_status(frame, chunks[3]);
	}

	fn draw_header(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
		let model = self.session.agent.model();
		let cwd_short = shorten_path(&self.cwd, 32);
		let line = Line::from(vec![
			Span::styled(
				format!(" {} ", crate::config::APP_NAME),
				Style::new().fg(self.theme.accent).add_modifier(Modifier::BOLD),
			),
			Span::styled(format!("v{} ", crate::config::VERSION), Style::new().fg(self.theme.muted)),
			Span::styled(model.key(), Style::new().fg(self.theme.tool)),
			Span::styled(format!("  {cwd_short}"), Style::new().fg(self.theme.muted)),
		]);
		frame.render_widget(Paragraph::new(line), area);
	}

	fn draw_editor(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.border))
			.title(Span::styled(
				if self.working { " steering/prompt (esc aborts) " } else { " prompt " },
				Style::new().fg(self.theme.muted),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);
		self.editor.render(inner, frame.buffer_mut());

		let cursor_x = inner.x + self.editor.cursor_screen_x(inner.width as usize);
		frame.set_cursor_position(Position { x: cursor_x.min(inner.right().saturating_sub(1)), y: inner.y });
	}

	fn draw_status(&mut self, frame: &mut Frame, area: ratatui::layout::Rect) {
		let (in_tokens, out_tokens, cost) = self.usage;
		let state = if self.working {
			let spinner = SPINNER[(self.tick / 4) as usize % SPINNER.len()];
			Span::styled(format!(" {spinner} working "), Style::new().fg(self.theme.accent))
		} else {
			Span::styled(" ○ idle ", Style::new().fg(self.theme.success))
		};
		let line = Line::from(vec![
			state,
			Span::styled(
				format!("· tok {in_tokens}/{} · ${cost:.4} ", out_tokens),
				Style::new().fg(self.theme.muted),
			),
			Span::styled("· /help · ctrl+c exit · esc abort · pgup/pgdn scroll", Style::new().fg(self.theme.muted)),
		]);
		frame.render_widget(Paragraph::new(line), area);
	}

	fn render_blocks(&self) -> Vec<Line<'static>> {
		let mut lines: Vec<Line<'static>> = Vec::new();
		for block in &self.blocks {
			match block {
				ChatBlock::User(text) => {
					lines.push(Line::from(Span::styled("❯ ", Style::new().fg(self.theme.accent))));
					for line in text.lines() {
						lines.push(Line::styled(format!("  {line}"), Style::new().fg(self.theme.user)));
					}
					lines.push(Line::from(""));
				}
				ChatBlock::Assistant { text, thinking, streaming, error } => {
					if !thinking.is_empty() {
						for line in thinking.lines().take(4) {
							lines.push(Line::styled(format!("  ~ {line}"), Style::new().fg(self.theme.thinking)));
						}
					}
					for line in text.lines() {
						lines.push(Line::styled(format!("  {line}"), Style::new().fg(self.theme.assistant)));
					}
					if *streaming {
						lines.push(Line::styled("  ▌", Style::new().fg(self.theme.accent)));
					}
					if let Some(error) = error {
						lines.push(Line::styled(format!("  error: {error}"), Style::new().fg(self.theme.error)));
					}
					lines.push(Line::from(""));
				}
				ChatBlock::Tool { name, args, running, ok, preview, .. } => {
					let glyph = if *running { "◌" } else if *ok { "✓" } else { "✗" };
					let color = if *running { self.theme.tool } else if *ok { self.theme.success } else { self.theme.error };
					lines.push(Line::from(vec![
						Span::styled(format!("  {glyph} "), Style::new().fg(color)),
						Span::styled(name.clone(), Style::new().fg(color).add_modifier(Modifier::BOLD)),
						Span::styled(format!(" {args}"), Style::new().fg(self.theme.muted)),
					]));
					if !preview.is_empty() {
						lines.push(Line::styled(format!("    {preview}"), Style::new().fg(self.theme.tool_output)));
					}
				}
				ChatBlock::Notice(text) => {
					for line in text.lines() {
						lines.push(Line::styled(format!("  {line}"), Style::new().fg(self.theme.muted).add_modifier(Modifier::ITALIC)));
					}
					lines.push(Line::from(""));
				}
			}
		}
		lines
	}
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn compact_args(args: &serde_json::Value) -> String {
	let s = args.to_string();
	let mut out = s.chars().take(60).collect::<String>();
	if s.chars().count() > 60 {
		out.push('…');
	}
	out
}

fn first_lines(text: &str, count: usize) -> String {
	let joined = text.lines().take(count).collect::<Vec<_>>().join(" ⏎ ");
	let mut out: String = joined.chars().take(120).collect();
	if out.chars().count() >= 120 || text.lines().count() > count {
		out.push('…');
	}
	out
}

fn shorten_path(path: &str, max: usize) -> String {
	let width = UnicodeWidthStr::width(path);
	if width <= max {
		return path.to_string();
	}
	let budget = max.saturating_sub(1); // leading ellipsis
	let mut tail = String::new();
	for c in path.chars().rev() {
		let char_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
		if UnicodeWidthStr::width(tail.as_str()) + char_width > budget {
			break;
		}
		tail.insert(0, c);
	}
	format!("…{tail}")
}

/// Wall-clock seed for tick; keeps the spinner animation frame-varying.
#[allow(dead_code)]
fn epoch_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
