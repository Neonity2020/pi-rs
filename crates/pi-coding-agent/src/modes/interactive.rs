//! Interactive mode — port of
//! `pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts`,
//! rendered with ratatui (pi-tui).
//!
//! Chat rendering follows pi's components: assistant/user messages render as
//! markdown (`components/assistant-message.ts` / `user-message.ts`), tool
//! calls use the `formatToolCallWithArgs` collapsed/expanded layout with a
//! 10-line output preview and ctrl+o expansion (`components/tool-execution.ts`,
//! `core/tools/render-utils.ts`), running tools preview their streaming output
//! tail, and the footer is the two-line `components/footer.ts` (cwd+branch /
//! token·cost·context% with the model right-aligned; context % comes from the
//! unified `core/context_usage.rs` algorithm with the `(auto)` compaction
//! indicator). `/model` opens the fuzzy model selector overlay
//! (`components/model-selector.ts`).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use pi_agent::AgentEvent;
use pi_ai::{Message, Model, StopReason};
use pi_tui::components::{Component, SelectAction, SelectItem, SelectList, markdown_lines};
use pi_tui::keybindings::KeybindingsManager;
use pi_tui::{Editor, ScrollView, TerminalGuard, Theme};

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle};
use crate::core::model_registry::{ModelRegistry, add_provider};
use crate::core::slash_commands::BUILTIN_SLASH_COMMANDS;
use crate::core::ui_config::UiConfig;

#[derive(Default)]
struct ProviderWizard {
	values: Vec<String>,
	input: String,
	error: Option<String>,
	selected: Option<usize>,
	cursor: usize,
}

const PROVIDER_OPTIONS: [&str; 4] = [
	"MiMo Token Plan (中国区，默认)",
	"OpenAI",
	"Anthropic",
	"自定义 OpenAI / Anthropic 兼容服务",
];

impl ProviderWizard {
	fn prompt(&self) -> &'static str {
		[
			"Provider ID (e.g. my-provider)",
			"API (openai-completions or anthropic-messages)",
			"Base URL (e.g. https://example.com/v1)",
			"Model ID",
			"API key (hidden; stored in models.json)",
		][self.values.len()]
	}
}

const TICK_MS: u64 = 80;
const TOOL_PREVIEW_LINES: usize = 10;
/// Running tools preview their live output tail (bash-execution.ts
/// `PREVIEW_LINES`), finished ones the first lines.
const TOOL_RUNNING_PREVIEW_LINES: usize = 20;
const COLLAPSED_ARGS_CHARS: usize = 100;
const EDITOR_MAX_ROWS: usize = 8;

/// Structured chat state; rendered to lines every frame. Markdown blocks keep
/// their last rendered output (`rendered: Option<(width, lines)>`, pi's
/// component invalidate pattern) so long sessions don't re-parse every frame.
enum ChatBlock {
	User(String),
	Assistant {
		text: String,
		thinking: String,
		streaming: bool,
		stop_note: Option<String>,
		error: Option<String>,
		rendered: Option<(usize, Vec<Line<'static>>)>,
	},
	Tool {
		id: String,
		name: String,
		args: serde_json::Value,
		running: bool,
		is_error: bool,
		/// Joined text items of the result content.
		output: String,
		started: Option<Instant>,
		duration_ms: Option<u128>,
		rendered: Option<(usize, Vec<Line<'static>>)>,
	},
	Notice(String),
}

struct ModelOverlay {
	search: String,
	list: SelectList,
}

impl ModelOverlay {
	fn new(models: &[Model], current_key: &str) -> Self {
		let items: Vec<SelectItem> = models
			.iter()
			.map(|m| {
				let mut desc = format!("[{}]", m.provider);
				if m.context_window > 0 {
					desc.push_str(&format!(" · {} ctx", format_tokens(m.context_window)));
				}
				if m.name != m.id {
					desc.push_str(&format!(" · {}", m.name));
				}
				SelectItem {
					value: m.key(),
					label: m.id.clone(),
					description: Some(desc),
				}
			})
			.collect();
		let mut list = SelectList::new(items, 10);
		list.set_no_match_message("No matching models");
		list.set_mark_value(Some(current_key.to_string()));
		Self {
			search: String::new(),
			list,
		}
	}
}

pub struct InteractiveMode {
	session: SessionHandle,
	models: Vec<Model>,
	registry: Arc<Mutex<ModelRegistry>>,
	provider_wizard: Option<ProviderWizard>,
	theme: Theme,
	// Resolved app keybindings (ui_config.rs): default table + user
	// keybindings.json overrides.
	keybindings: KeybindingsManager,
	// Config notices (bad keybindings.json / theme file) shown at startup.
	startup_notices: Vec<String>,
	editor: Editor,
	chat: ScrollView,
	blocks: Vec<ChatBlock>,
	working: bool,
	quit: bool,
	tick: u64,
	cwd: String,
	// Footer data (footer.ts): usage totals, git branch. Context usage is not
	// cached here — it comes from the unified core::context_usage algorithm.
	usage: (u64, u64, u64, u64, f64), // input, output, cache_read, cache_write, cost
	branch: Option<String>,
	branch_refresh_tick: u64,
	// Toggles (pi keybindings app.tools.expand / app.thinking.toggle).
	tools_expanded: bool,
	thinking_expanded: bool,
	// Model selector overlay.
	overlay: Option<ModelOverlay>,
	// UI-only messages from background tasks (git branch watcher).
	ui_rx: mpsc::UnboundedReceiver<String>,
	ui_tx: mpsc::UnboundedSender<String>,
}

pub fn format_tokens(count: u64) -> String {
	if count < 1_000 {
		count.to_string()
	} else if count < 10_000 {
		format!("{:.1}k", count as f64 / 1_000.0)
	} else if count < 1_000_000 {
		format!("{}k", count / 1_000)
	} else if count < 10_000_000 {
		format!("{:.1}M", count as f64 / 1_000_000.0)
	} else {
		format!("{}M", count / 1_000_000)
	}
}

/// Duration as `4.2s` below a minute, then `1m 30s` (render-utils.ts).
pub fn format_duration(ms: u128) -> String {
	let seconds = ms as f64 / 1000.0;
	if seconds < 60.0 {
		return format!("{seconds:.1}s");
	}
	let total = seconds as u128;
	let minutes = total / 60;
	let rem = total % 60;
	if minutes < 60 {
		format!("{minutes}m {rem}s")
	} else {
		let hours = minutes / 60;
		format!("{hours}h {}m {rem}s", minutes % 60)
	}
}

/// Footer context segment (footer.ts render): `71.4%/200k`, or `?/200k` while
/// usage is unknown (after compaction, before the next response) with the
/// window still taken from the model. The `(auto)` indicator follows the
/// window while auto-compaction is enabled. Color thresholds are strict:
/// >90% error, >70% warning, unknown stays neutral.
fn footer_context_segment(
	usage: Option<(u64, u64)>,
	model_window: u64,
	auto_compaction: bool,
	theme: &Theme,
) -> (String, Style) {
	let window = usage.map(|(_, window)| window).unwrap_or(model_window);
	let auto = if auto_compaction { " (auto)" } else { "" };
	let Some((tokens, _)) = usage else {
		return (
			format!("?/{}{auto}", format_tokens(window)),
			Style::default(),
		);
	};
	let percent = tokens as f64 / window as f64 * 100.0;
	let style = if percent > 90.0 {
		Style::new().fg(theme.error)
	} else if percent > 70.0 {
		Style::new().fg(theme.tool) // warning slot
	} else {
		Style::default()
	};
	(
		format!("{percent:.1}%/{}{auto}", format_tokens(window)),
		style,
	)
}

/// The app-level actions pi-rs interactive dispatches. The definition table
/// (ui_config.rs) carries upstream's full app action list; the ones pi-rs
/// has not implemented stay in the table but resolve to nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalAction {
	Interrupt,
	Exit,
	Clear,
	ModelSelect,
	ToolsExpand,
	ThinkingToggle,
}

/// Resolve a key to a global action, mirroring upstream
/// `custom-editor.ts` `handleInput` order: app.interrupt, app.exit (only
/// with an empty editor), then the other registered app actions. Per-action
/// `matches` instead of `action_for_event` — the merged table also holds
/// `tui.*` actions whose defaults shadow app chords (escape is
/// `tui.select.cancel`, ctrl+d `tui.editor.deleteCharForward`, ctrl+c
/// `tui.input.copy`), and upstream checks `app.interrupt` before all of
/// them. Pure so the keybindings wiring is testable without a TTY.
fn resolve_global_action(
	keybindings: &KeybindingsManager,
	key: &KeyEvent,
	editor_is_empty: bool,
) -> Option<GlobalAction> {
	if keybindings.matches("app.interrupt", key) {
		return Some(GlobalAction::Interrupt);
	}
	// app.exit only exits with an empty editor; with content, upstream falls
	// through (the editor deletes a char forward) and the remaining app
	// actions still get their chance.
	if keybindings.matches("app.exit", key) && editor_is_empty {
		return Some(GlobalAction::Exit);
	}
	if keybindings.matches("app.clear", key) {
		return Some(GlobalAction::Clear);
	}
	if keybindings.matches("app.model.select", key) {
		return Some(GlobalAction::ModelSelect);
	}
	if keybindings.matches("app.tools.expand", key) {
		return Some(GlobalAction::ToolsExpand);
	}
	if keybindings.matches("app.thinking.toggle", key) {
		return Some(GlobalAction::ThinkingToggle);
	}
	None
}

impl InteractiveMode {
	pub fn new(
		session: SessionHandle,
		models: Vec<Model>,
		registry: Arc<Mutex<ModelRegistry>>,
		cwd: String,
		ui: UiConfig,
	) -> Self {
		let (ui_tx, ui_rx) = mpsc::unbounded_channel();
		let theme = ui.theme;
		Self {
			session,
			models,
			registry,
			provider_wizard: None,
			theme: theme.clone(),
			keybindings: ui.keybindings,
			startup_notices: ui.notices,
			editor: Editor::new("❯ ").with_style(Style::new().fg(theme.assistant)),
			chat: ScrollView::new(),
			blocks: Vec::new(),
			working: false,
			quit: false,
			tick: 0,
			cwd,
			usage: (0, 0, 0, 0, 0.0),
			branch: None,
			branch_refresh_tick: 0,
			tools_expanded: false,
			thinking_expanded: true,
			overlay: None,
			ui_rx,
			ui_tx,
		}
	}

	pub async fn run(mut self, initial_prompt: Option<String>) -> anyhow::Result<()> {
		let mut guard = TerminalGuard::new()?;

		// Config problems first (keybindings.json / theme load failures), then
		// the welcome banner with the effective bindings.
		for notice in std::mem::take(&mut self.startup_notices) {
			self.push_notice(notice);
		}
		self.push_notice(format!(
			"{} v{} — type /help for commands; {} exits, {} aborts a run.",
			crate::config::APP_NAME,
			crate::config::VERSION,
			self.keybinding_hint("app.clear", "ctrl+c"),
			self.keybinding_hint("app.interrupt", "escape"),
		));

		self.refresh_branch();

		if let Some(prompt) = initial_prompt
			&& !prompt.trim().is_empty()
		{
			self.send_prompt(prompt);
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
				Some(branch) = self.ui_rx.recv() => {
					self.branch = Some(branch);
				}
				_ = tokio::time::sleep(Duration::from_millis(TICK_MS)) => {
					self.tick = self.tick.wrapping_add(1);
					self.branch_refresh_tick += 1;
					if self.branch_refresh_tick >= 375 {
						self.branch_refresh_tick = 0;
						self.refresh_branch();
					}
				}
			}

			if self.quit {
				break;
			}
		}

		let _ = self.session.cmd_tx.send(SessionCommand::Exit);
		while !matches!(
			self.session.events.recv().await,
			Err(tokio::sync::broadcast::error::RecvError::Closed)
		) {}
		Ok(())
	}

	// -- input handling -------------------------------------------------------

	fn handle_terminal_event(&mut self, event: TermEvent) {
		if let (Some(wizard), TermEvent::Paste(text)) = (&mut self.provider_wizard, &event) {
			if wizard.selected.is_some() {
				wizard.input.push_str(&text.replace(['\r', '\n'], ""));
			}
			return;
		}
		if let TermEvent::Key(key) = &event {
			if key.kind == crossterm::event::KeyEventKind::Release {
				return;
			}
			if self.provider_wizard.is_some() {
				self.handle_provider_key(*key);
				return;
			}
			if self.overlay.is_some() {
				self.handle_overlay_key(*key);
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

	fn handle_provider_key(&mut self, key: KeyEvent) {
		let wizard = self.provider_wizard.as_mut().unwrap();
		if wizard.selected.is_none() {
			match key.code {
				KeyCode::Up => wizard.cursor = wizard.cursor.saturating_sub(1),
				KeyCode::Down => {
					wizard.cursor = (wizard.cursor + 1).min(PROVIDER_OPTIONS.len() - 1)
				}
				KeyCode::Esc => self.provider_wizard = None,
				KeyCode::Enter => {
					wizard.selected = Some(wizard.cursor);
					let preset = match wizard.cursor {
						0 => Some(("mimo", "openai-completions")),
						1 => Some(("openai", "openai-completions")),
						2 => Some(("anthropic", "anthropic-messages")),
						_ => None,
					};
					if let Some((id, api)) = preset {
						wizard.values = vec![id.into(), api.into(), String::new(), String::new()];
					}
				}
				_ => {}
			}
			return;
		}
		match key.code {
			KeyCode::Esc => {
				self.provider_wizard = None;
				self.push_notice("Provider setup cancelled".into());
			}
			KeyCode::Backspace => {
				wizard.input.pop();
			}
			KeyCode::Char(c)
				if !key
					.modifiers
					.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
			{
				wizard.input.push(c)
			}
			KeyCode::Enter => {
				if wizard.input.trim().is_empty() {
					wizard.error = Some("A value is required".into());
					return;
				}
				if wizard.values.len() == 1
					&& wizard.selected == Some(3)
					&& !matches!(
						wizard.input.trim(),
						"openai-completions" | "anthropic-messages"
					) {
					wizard.error = Some("Use openai-completions or anthropic-messages".into());
					return;
				}
				wizard.error = None;
				wizard
					.values
					.push(std::mem::take(&mut wizard.input).trim().to_string());
				if wizard.values.len() == 5 {
					let wizard = self.provider_wizard.take().unwrap();
					let v = &wizard.values;
					match add_provider(
						&crate::config::models_json_path(),
						&v[0],
						&v[1],
						&v[2],
						&v[3],
						&v[4],
					) {
						Ok(()) => {
							let mut registry = self.registry.lock().unwrap();
							let default = registry.default_model();
							*registry = ModelRegistry::load(Some(&default.key()));
							self.models = registry.models().to_vec();
							drop(registry);
							let model = match wizard.selected {
								Some(0) => "mimo/mimo-v2.6-pro".to_string(),
								Some(1) => "openai/gpt-5.2".to_string(),
								Some(2) => "anthropic/claude-sonnet-4-6".to_string(),
								_ => format!("{}/{}", v[0], v[3]),
							};
							let _ = self.session.cmd_tx.send(SessionCommand::SetModel {
								query: model.clone(),
							});
							self.push_notice(format!(
								"Provider {} saved. Switching to {}.",
								v[0], model
							));
						}
						Err(error) => self.push_notice(format!("Provider was not saved: {error}")),
					}
				}
			}
			_ => {}
		}
	}

	fn handle_overlay_key(&mut self, key: KeyEvent) {
		// Search-line editing first (printable chars and backspace). Upstream
		// drives the search input through the editor, so its keys follow the
		// tui.editor.* bindings; the pi-rs Editor does not consult the
		// keybindings manager, so this line stays hardcoded — unlike the list
		// actions below, which go through the table.
		if !key.modifiers.contains(KeyModifiers::CONTROL)
			&& !key.modifiers.contains(KeyModifiers::ALT)
		{
			let Some(overlay) = self.overlay.as_mut() else {
				return;
			};
			match key.code {
				KeyCode::Char(c) => {
					overlay.search.push(c);
					overlay.list.set_filter(overlay.search.clone());
					return;
				}
				KeyCode::Backspace => {
					overlay.search.pop();
					overlay.list.set_filter(overlay.search.clone());
					return;
				}
				_ => {}
			}
		}
		// List navigation/confirm/cancel via the keybindings manager
		// (upstream model-selector.ts asks it for tui.select.* / tui.input.tab
		// / app.models.save; pi-rs dispatches the select actions only).
		let action = match self.overlay.as_mut() {
			Some(overlay) => overlay.list.handle_key_with(key, &self.keybindings),
			None => return,
		};
		match action {
			SelectAction::Select(index) => {
				let value = match self.overlay.as_ref() {
					Some(overlay) => overlay
						.list
						.filtered()
						.get(index)
						.map(|item| item.value.clone()),
					None => None,
				};
				if let Some(value) = value {
					self.overlay = None;
					let _ = self
						.session
						.cmd_tx
						.send(SessionCommand::SetModel { query: value });
				}
				// No match under the cursor: keep the overlay open (pi behavior).
			}
			SelectAction::Cancel => self.overlay = None,
			SelectAction::Handled | SelectAction::Ignored => {}
		}
	}

	fn handle_global_key(&mut self, key: KeyEvent) -> bool {
		let Some(action) = resolve_global_action(&self.keybindings, &key, self.editor.is_empty())
		else {
			return false;
		};
		match action {
			GlobalAction::Interrupt => {
				// Escape always aborts/consumes (pi behavior), even idle.
				if self.working {
					let _ = self.session.cmd_tx.send(SessionCommand::Abort);
				}
			}
			// pi divergence: upstream app.clear clears the editor (double
			// press exits); pi-rs keeps its simpler ctrl+c exits.
			GlobalAction::Clear | GlobalAction::Exit => self.quit = true,
			GlobalAction::ModelSelect => {
				let current = self.session.agent.model().key();
				self.overlay = Some(ModelOverlay::new(&self.models, &current));
			}
			GlobalAction::ToolsExpand => {
				// app.tools.expand (tool-execution.ts): toggle full tool output.
				self.tools_expanded = !self.tools_expanded;
				for block in &mut self.blocks {
					if let ChatBlock::Tool { rendered, .. } = block {
						*rendered = None;
					}
				}
			}
			GlobalAction::ThinkingToggle => {
				// app.thinking.toggle: show/hide thinking blocks.
				self.thinking_expanded = !self.thinking_expanded;
				for block in &mut self.blocks {
					if let ChatBlock::Assistant { rendered, .. } = block {
						*rendered = None;
					}
				}
			}
		}
		true
	}

	/// The action's effective bindings as a hint string (`ctrl+o/ctrl+p`
	/// style), falling back to the default chord when unbound.
	fn keybinding_hint(&self, action: &str, fallback: &str) -> String {
		match self.keybindings.formatted_keys(action) {
			Some(keys) if !keys.is_empty() => keys.join("/"),
			_ => fallback.to_string(),
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
			"provider" | "login" => self.provider_wizard = Some(ProviderWizard::default()),
			"help" | "h" | "?" => {
				let mut text = String::from("commands:\n");
				for info in BUILTIN_SLASH_COMMANDS {
					text.push_str(&format!("  /{:<10} {}\n", info.name, info.description));
				}
				// Hints reflect keybindings.json (upstream renders hints from
				// the keybindings manager too).
				text.push_str(&format!(
					"\nkeys: {} expand tool output · {} toggle thinking · pgup/pgdn scroll",
					self.keybinding_hint("app.tools.expand", "ctrl+o"),
					self.keybinding_hint("app.thinking.toggle", "ctrl+t"),
				));
				self.push_notice(text);
			}
			"exit" | "quit" | "q" => self.quit = true,
			"clear" => {
				self.blocks.clear();
				self.chat.clear();
			}
			"model" => {
				if args.is_empty() {
					let current = self.session.agent.model().key();
					self.overlay = Some(ModelOverlay::new(&self.models, &current));
				} else {
					let _ = self.session.cmd_tx.send(SessionCommand::SetModel {
						query: args.to_string(),
					});
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
				self.push_notice(format!(
					"system prompt (first 30 lines):\n{}\n…",
					preview.join("\n")
				));
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
								dir.file_name()
									.map(|n| n.to_string_lossy().into_owned())
									.unwrap_or_default(),
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
			SessionEvent::Agent(event) => self.handle_agent_event(*event),
			SessionEvent::Notice(text) => self.push_notice(text),
			SessionEvent::Persisted { .. } => {}
			// Quiescence marker; the TUI derives busy/idle from agent events.
			SessionEvent::Settled { .. } => {}
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
				self.refresh_branch();
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
							stop_note: None,
							error: None,
							rendered: None,
						});
					}
					_ => {}
				}
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::MessageUpdate { message, .. } => {
				if let Message::Assistant(assistant) = &message
					&& let Some(ChatBlock::Assistant {
						text,
						thinking,
						rendered,
						..
					}) = self.blocks.iter_mut().rev().find(|b| {
						matches!(
							b,
							ChatBlock::Assistant {
								streaming: true,
								..
							}
						)
					}) {
					*text = assistant.text();
					*thinking = assistant.thinking();
					*rendered = None;
				}
			}
			AgentEvent::MessageEnd { message } => {
				if let Message::Assistant(assistant) = &message {
					if let Some(ChatBlock::Assistant {
						streaming,
						stop_note,
						error,
						rendered,
						..
					}) = self.blocks.iter_mut().rev().find(|b| {
						matches!(
							b,
							ChatBlock::Assistant {
								streaming: true,
								..
							}
						)
					}) {
						*streaming = false;
						*rendered = None;
						*error = assistant.error_message.clone();
						// stopReason notes (assistant-message.ts): length truncation,
						// aborted runs and errors surface after the partial content.
						*stop_note = match &assistant.stop_reason {
							StopReason::Length => {
								Some("Response was truncated before completion.".into())
							}
							StopReason::Aborted => Some(match assistant.error_message.as_deref() {
								Some(e) if e != "Request was aborted" => e.to_string(),
								_ => "Operation aborted".into(),
							}),
							StopReason::Error => Some(format!(
								"Error: {}",
								assistant
									.error_message
									.as_deref()
									.unwrap_or("Unknown error")
							)),
							_ => None,
						};
					}
					let (input, output, cache_read, cache_write, cost) = &mut self.usage;
					*input += assistant.usage.input;
					*output += assistant.usage.output;
					*cache_read += assistant.usage.cache_read;
					*cache_write += assistant.usage.cache_write;
					*cost += assistant.usage.cost.total.unwrap_or(0.0);
				}
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::ToolExecutionStart {
				tool_call_id,
				tool_name,
				args,
			} => {
				self.blocks.push(ChatBlock::Tool {
					id: tool_call_id,
					name: tool_name,
					args,
					running: true,
					is_error: false,
					output: String::new(),
					started: Some(Instant::now()),
					duration_ms: None,
					rendered: None,
				});
				self.chat.scroll_to_bottom(usize::MAX);
			}
			AgentEvent::ToolExecutionUpdate {
				tool_call_id,
				partial_result,
				..
			} => {
				// Streaming deltas append to the running block's output so the
				// preview follows the live stream; no result cache to clear
				// beyond the running block's (never cached anyway).
				apply_tool_update(&mut self.blocks, &tool_call_id, &partial_result);
			}
			AgentEvent::ToolExecutionEnd {
				tool_call_id,
				result,
				is_error: is_error_,
				..
			} => {
				let output = tool_result_text(&result);
				if let Some(ChatBlock::Tool {
					running,
					is_error,
					output: out,
					started,
					duration_ms,
					rendered,
					..
				}) = self.blocks.iter_mut().rev().find(
					|b| matches!(b, ChatBlock::Tool { id, running: true, .. } if *id == tool_call_id),
				) {
					*running = false;
					*is_error = is_error_;
					*rendered = None;
					*out = output;
					*duration_ms = started.map(|s| s.elapsed().as_millis());
				}
			}
			AgentEvent::TurnStart | AgentEvent::TurnEnd { .. } => {}
		}
	}

	fn push_notice(&mut self, text: String) {
		self.blocks.push(ChatBlock::Notice(text));
		self.chat.scroll_to_bottom(usize::MAX);
	}

	// -- git branch (footer data provider) ---------------------------------------

	fn refresh_branch(&self) {
		let tx = self.ui_tx.clone();
		tokio::spawn(async move {
			let output = tokio::process::Command::new("git")
				.args(["rev-parse", "--abbrev-ref", "HEAD"])
				.current_dir(std::env::current_dir().unwrap_or_default())
				.output()
				.await;
			if let Ok(output) = output {
				let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
				if output.status.success() && !name.is_empty() {
					let _ = tx.send(name);
				}
			}
		});
	}

	// -- rendering ---------------------------------------------------------------

	fn draw(&mut self, frame: &mut Frame) {
		let area = frame.area();
		let editor_rows = self
			.editor
			.preferred_height(area.width.saturating_sub(2))
			.clamp(1, EDITOR_MAX_ROWS as u16);
		let chunks = Layout::vertical([
			Constraint::Length(1),
			Constraint::Min(3),
			Constraint::Length(editor_rows + 2),
			Constraint::Length(2),
		])
		.split(area);

		self.draw_header(frame, chunks[0]);
		let chat_width = chunks[1].width.saturating_sub(2) as usize;
		let lines = self.render_blocks(chat_width);
		self.chat.set_lines(lines);
		self.chat.render(chunks[1], frame.buffer_mut());
		self.draw_editor(frame, chunks[2]);
		self.draw_footer(frame, chunks[3]);
		self.draw_overlay(frame, chunks[1]);
		if let Some(wizard) = &self.provider_wizard {
			let area = chunks[1];
			let height = area
				.height
				.min(if wizard.selected.is_none() { 8 } else { 6 });
			let panel = Rect {
				x: area.x,
				y: area.y,
				width: area.width,
				height,
			};
			frame.render_widget(Clear, panel);
			let input = if wizard.values.len() == 4 {
				"*".repeat(wizard.input.chars().count())
			} else {
				wizard.input.clone()
			};
			let text = if wizard.selected.is_none() {
				let mut text = PROVIDER_OPTIONS
					.iter()
					.enumerate()
					.map(|(i, label)| {
						format!("{} {}", if i == wizard.cursor { "❯" } else { " " }, label)
					})
					.collect::<Vec<_>>()
					.join("\n");
				text.push_str("\n↑/↓ 选择 · Enter 确认 · Esc 取消");
				text
			} else {
				format!(
					"{}\n{}▌\n{}\nEnter: next/save · Esc: cancel",
					wizard.prompt(),
					input,
					wizard.error.as_deref().unwrap_or("")
				)
			};
			frame.render_widget(
				Paragraph::new(text).block(Block::bordered().title(" Add provider ")),
				panel,
			);
		}
	}

	fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
		let model = self.session.agent.model();
		let cwd_short = shorten_path(&self.cwd, 32);
		let line = Line::from(vec![
			Span::styled(
				format!(" {} ", crate::config::APP_NAME),
				Style::new()
					.fg(self.theme.accent)
					.add_modifier(Modifier::BOLD),
			),
			Span::styled(
				format!("v{} ", crate::config::VERSION),
				Style::new().fg(self.theme.muted),
			),
			Span::styled(model.key(), Style::new().fg(self.theme.tool)),
			Span::styled(format!("  {cwd_short}"), Style::new().fg(self.theme.muted)),
		]);
		frame.render_widget(Paragraph::new(line), area);
	}

	fn draw_editor(&mut self, frame: &mut Frame, area: Rect) {
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.border))
			.title(Span::styled(
				if self.working {
					" steering/prompt (esc aborts) "
				} else {
					" prompt "
				},
				Style::new().fg(self.theme.muted),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);
		self.editor.render(inner, frame.buffer_mut());

		let (x, y) = self.editor.cursor_screen_pos(inner);
		frame.set_cursor_position(Position {
			x: inner.x + x.min(inner.width.saturating_sub(1)),
			y: inner.y + y.min(inner.height.saturating_sub(1)),
		});
	}

	/// Two-line footer (footer.ts): cwd (+git branch), then token/cost/context
	/// stats with the model right-aligned. The context segment shows `?` while
	/// usage is unknown and the `(auto)` indicator while auto-compaction is on.
	fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
		let width = area.width as usize;
		if width == 0 {
			return;
		}

		// Line 1: pwd with ~ substitution and git branch.
		let home = dirs::home_dir();
		let pwd = match &home {
			Some(home) if self.cwd.starts_with(home.to_string_lossy().as_ref()) => {
				format!("~{}", &self.cwd[home.to_string_lossy().len()..])
			}
			_ => self.cwd.clone(),
		};
		let pwd = match &self.branch {
			Some(branch) => format!("{pwd} ({branch})"),
			None => pwd,
		};
		let pwd_line = Line::from(Span::styled(
			truncate_to_width(&pwd, width),
			Style::new().fg(self.theme.muted),
		));
		frame.render_widget(Paragraph::new(pwd_line), area);

		// Line 2: stats.
		let (input, output, cache_read, cache_write, cost) = self.usage;
		let mut stats: Vec<String> = Vec::new();
		if input > 0 {
			stats.push(format!("↑{}", format_tokens(input)));
		}
		if output > 0 {
			stats.push(format!("↓{}", format_tokens(output)));
		}
		if cache_read > 0 {
			stats.push(format!("R{}", format_tokens(cache_read)));
		}
		if cache_write > 0 {
			stats.push(format!("W{}", format_tokens(cache_write)));
		}
		if cost > 0.0 {
			stats.push(format!("${cost:.3}"));
		}

		// Context usage via the unified algorithm (core/context_usage.rs, the
		// footer.ts getSessionStats path): provider usage preferred with
		// trailing estimation; None after compaction until the next response.
		let usage = crate::core::context_usage::compute(&self.session.agent);
		let model_window = self.session.agent.model().context_window;
		let auto_compaction = self.session.auto_compaction.load(Ordering::Relaxed);
		let (context_text, context_style) =
			footer_context_segment(usage, model_window, auto_compaction, &self.theme);

		let model = self.session.agent.model();
		let thinking = self.session.agent.thinking_level();
		let mut right = model.id.clone();
		if model.reasoning {
			right.push_str(&format!(" • thinking {}", thinking_label(thinking)));
		}
		if self.models.len() > 1 {
			right = format!("({}) {}", model.provider, right);
		}

		let mut left = stats.join(" ");
		if !left.is_empty() {
			left.push(' ');
		}
		let left_width = left.width();
		let context_width = context_text.width();
		let right_width = right.width();
		let stats_line = if left_width + context_width + 2 + right_width <= width {
			let padding = width - left_width - context_width - right_width;
			let mut spans = vec![Span::styled(left, Style::new().fg(self.theme.muted))];
			spans.push(Span::styled(context_text, context_style));
			spans.push(Span::raw(" ".repeat(padding)));
			spans.push(Span::styled(right, Style::new().fg(self.theme.muted)));
			Line::from(spans)
		} else {
			Line::from(Span::styled(
				truncate_to_width(&format!("{left}{}", context_text), width),
				Style::new().fg(self.theme.muted),
			))
		};
		let stats_area = Rect {
			y: area.y + 1,
			height: 1,
			..area
		};
		frame.render_widget(Paragraph::new(stats_line), stats_area);
	}

	/// Model selector overlay (model-selector.ts): bordered panel with a search
	/// line, the filtered list and a key hint.
	fn draw_overlay(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(overlay) = self.overlay.as_ref() else {
			return;
		};
		let width = chat_area.width.clamp(24, 72);
		let list_height = 10usize;
		let height = (list_height + 6) as u16;
		if chat_area.width < width || chat_area.height < height {
			return;
		}
		let x = chat_area.x + (chat_area.width - width) / 2;
		let y = chat_area.y + (chat_area.height - height) * 2 / 5;
		let area = Rect {
			x,
			y,
			width,
			height,
		};

		frame.render_widget(Clear, area);
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.accent))
			.title(Span::styled(
				" Select model ",
				Style::new()
					.fg(self.theme.accent)
					.add_modifier(Modifier::BOLD),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);

		let search_line = Line::from(vec![
			Span::styled("search: ", Style::new().fg(self.theme.muted)),
			Span::raw(overlay.search.clone()),
			Span::styled("▌", Style::new().fg(self.theme.accent)),
		]);
		let list_lines = overlay.list.render_lines(inner.width as usize, &self.theme);
		let mut lines = vec![search_line, Line::default()];
		lines.extend(list_lines);
		lines.push(Line::from(Span::styled(
			format!(
				" {} select · {} cancel · type to filter",
				self.keybinding_hint("tui.select.confirm", "enter"),
				self.keybinding_hint("tui.select.cancel", "esc"),
			),
			Style::new().fg(self.theme.muted),
		)));
		// Pad/trim to the inner height.
		lines.resize(inner.height as usize, Line::default());
		frame.render_widget(Paragraph::new(lines), inner);
	}

	fn render_blocks(&mut self, width: usize) -> Vec<Line<'static>> {
		let theme = self.theme.clone();
		let tools_expanded = self.tools_expanded;
		let thinking_expanded = self.thinking_expanded;
		// Block-level hints follow keybindings.json (fallback: the defaults).
		let expand_hint = self.keybinding_hint("app.tools.expand", "ctrl+o");
		let thinking_hint = self.keybinding_hint("app.thinking.toggle", "ctrl+t");
		let tick = self.tick;
		let mut lines: Vec<Line<'static>> = Vec::new();
		for block in &mut self.blocks {
			match block {
				ChatBlock::User(text) => {
					// user-message.ts: markdown block in the user message style.
					for line in markdown_lines(text, width, &theme) {
						lines.push(patch_line(line, |s| s.fg(theme.user).bg(theme.user_bg)));
					}
					lines.push(Line::default());
				}
				ChatBlock::Assistant {
					text,
					thinking,
					streaming,
					stop_note,
					error,
					rendered,
				} => {
					if let Some((w, cached)) = rendered
						&& *w == width
					{
						lines.extend(cached.iter().cloned());
						continue;
					}
					let out = render_assistant_block(
						text,
						thinking,
						*streaming,
						stop_note.as_deref(),
						error.as_deref(),
						width,
						&theme,
						thinking_expanded,
						&thinking_hint,
					);
					// Cache finished blocks only; streaming ones invalidate anyway.
					if !*streaming {
						*rendered = Some((width, out.clone()));
					}
					lines.extend(out);
				}
				ChatBlock::Tool {
					name,
					args,
					running,
					is_error,
					output,
					started,
					duration_ms,
					rendered,
					..
				} => {
					if !*running
						&& let Some((w, cached)) = rendered
						&& *w == width
					{
						lines.extend(cached.iter().cloned());
						continue;
					}
					// Live elapsed while running (render-utils.ts toolTimerText
					// partial path); the final recorded duration wins.
					let shown_ms =
						(*duration_ms).or_else(|| started.map(|s| s.elapsed().as_millis()));
					let out = render_tool_block(
						name,
						args,
						*running,
						*is_error,
						output,
						shown_ms,
						width,
						&theme,
						tools_expanded,
						tick,
						&expand_hint,
					);
					if !*running {
						*rendered = Some((width, out.clone()));
					}
					lines.extend(out);
				}
				ChatBlock::Notice(text) => {
					for line in text.lines() {
						lines.push(Line::styled(
							line.to_string(),
							Style::new().fg(theme.muted).add_modifier(Modifier::ITALIC),
						));
					}
					lines.push(Line::default());
				}
			}
		}
		lines
	}
}

fn tool_result_text(result: &serde_json::Value) -> String {
	result
		.get("content")
		.and_then(|c| c.as_array())
		.map(|items| {
			items
				.iter()
				.filter_map(|item| {
					item.as_str()
						.or_else(|| item.get("text").and_then(|v| v.as_str()))
				})
				.collect::<Vec<_>>()
				.join("\n")
		})
		.unwrap_or_default()
}

/// Text delta of a streaming update: the `text` field of the
/// `{"text": "...", "isError": bool}` partial payload (agent_loop
/// ToolExecutionUpdate), falling back to final-result content parsing.
fn partial_output_text(partial_result: &serde_json::Value) -> Option<String> {
	if let Some(text) = partial_result.get("text").and_then(|v| v.as_str()) {
		return Some(text.to_string());
	}
	let text = tool_result_text(partial_result);
	(!text.is_empty()).then_some(text)
}

/// Append a streaming update to the running tool block with the matching call
/// id (bash streams chunked output; the preview shows the live tail). Already
/// finished blocks and unknown ids are ignored. Returns whether it applied.
fn apply_tool_update(
	blocks: &mut [ChatBlock],
	tool_call_id: &str,
	partial_result: &serde_json::Value,
) -> bool {
	let Some(delta) = partial_output_text(partial_result) else {
		return false;
	};
	let Some(ChatBlock::Tool {
		output, rendered, ..
	}) = blocks
		.iter_mut()
		.rev()
		.find(|b| matches!(b, ChatBlock::Tool { id, running: true, .. } if *id == tool_call_id))
	else {
		return false;
	};
	// Progress bars rewrite lines with \r; the plain-text preview drops the
	// carriage returns (render-utils.ts normalizeDisplayText).
	output.push_str(&delta.replace('\r', ""));
	*rendered = None;
	true
}

/// Assistant message lines (assistant-message.ts): thinking block first
/// (ctrl+t collapses it to a label), then the markdown text, then stopReason
/// notes for truncation/abort/error.
#[allow(clippy::too_many_arguments)]
fn render_assistant_block(
	text: &str,
	thinking: &str,
	streaming: bool,
	stop_note: Option<&str>,
	error: Option<&str>,
	width: usize,
	theme: &Theme,
	thinking_expanded: bool,
	toggle_hint: &str,
) -> Vec<Line<'static>> {
	let mut lines: Vec<Line<'static>> = Vec::new();
	if !thinking.trim().is_empty() {
		if thinking_expanded {
			lines.push(Line::from(Span::styled(
				format!("· thinking ({toggle_hint} to hide)"),
				Style::new()
					.fg(theme.thinking)
					.add_modifier(Modifier::ITALIC),
			)));
			for line in markdown_lines(thinking.trim(), width, theme) {
				lines.push(patch_line(line, |s| {
					s.fg(theme.thinking).add_modifier(Modifier::ITALIC)
				}));
			}
		} else {
			lines.push(Line::from(Span::styled(
				format!("· thinking hidden ({toggle_hint} to show)"),
				Style::new()
					.fg(theme.thinking)
					.add_modifier(Modifier::ITALIC),
			)));
		}
	}
	if !text.trim().is_empty() {
		lines.extend(markdown_lines(text.trim(), width, theme));
	}
	if streaming {
		lines.push(Line::from(Span::styled("▌", Style::new().fg(theme.accent))));
	}
	if let Some(note) = stop_note {
		lines.push(Line::from(Span::styled(
			note.to_string(),
			Style::new().fg(theme.error),
		)));
	}
	if let Some(error) = error {
		lines.push(Line::from(Span::styled(
			format!("Error: {error}"),
			Style::new().fg(theme.error),
		)));
	}
	lines.push(Line::default());
	lines
}

/// Tool execution lines (tool-execution.ts + render-utils.ts): state glyph,
/// bold tool name, duration, collapsed `key=value` args or expanded `key:
/// value` lines, and a 10-line output preview with an expand hint.
#[allow(clippy::too_many_arguments)]
fn render_tool_block(
	name: &str,
	args: &serde_json::Value,
	running: bool,
	is_error: bool,
	output: &str,
	duration_ms: Option<u128>,
	width: usize,
	theme: &Theme,
	tools_expanded: bool,
	tick: u64,
	expand_hint: &str,
) -> Vec<Line<'static>> {
	let _ = width;
	let mut lines: Vec<Line<'static>> = Vec::new();
	let (glyph, color) = if running {
		(SPINNER[(tick / 4) as usize % SPINNER.len()], theme.tool)
	} else if is_error {
		("✗", theme.error)
	} else {
		("✓", theme.success)
	};
	let title = Style::new().fg(color).add_modifier(Modifier::BOLD);
	let mut spans = vec![
		Span::styled(format!("{glyph} "), Style::new().fg(color)),
		Span::styled(name.to_string(), title),
	];
	if let Some(ms) = duration_ms {
		spans.push(Span::styled(
			format!(" {}", format_duration(ms)),
			Style::new().fg(theme.muted),
		));
	}
	if !tools_expanded {
		// Collapsed: `key=value` pairs on the title line, cut to
		// COLLAPSED_ARGS_CHARS (formatToolCallWithArgs).
		let pairs = compact_args_pairs(args);
		if !pairs.is_empty() {
			spans.push(Span::styled(
				format!(" {pairs}"),
				Style::new().fg(theme.muted),
			));
		}
		lines.push(Line::from(spans));
	} else {
		// Expanded: one `key: value` line per argument.
		lines.push(Line::from(spans));
		if let Some(obj) = args.as_object() {
			for (key, value) in obj {
				let text = match value {
					serde_json::Value::String(s) => s.clone(),
					other => other.to_string(),
				};
				for (i, part) in text.lines().enumerate() {
					let prefix = if i == 0 {
						format!("  {key}: ")
					} else {
						"    ".to_string()
					};
					lines.push(Line::from(Span::styled(
						format!("{prefix}{part}"),
						Style::new().fg(theme.muted),
					)));
				}
			}
		}
	}

	if !output.is_empty() {
		let out_lines: Vec<&str> = output.lines().collect();
		let preview_lines = if running {
			TOOL_RUNNING_PREVIEW_LINES
		} else {
			TOOL_PREVIEW_LINES
		};
		let shown = if tools_expanded {
			out_lines.len()
		} else {
			out_lines.len().min(preview_lines)
		};
		let out_style = if is_error {
			Style::new().fg(theme.error)
		} else {
			Style::new().fg(theme.tool_output)
		};
		let start = if running && !tools_expanded {
			out_lines.len() - shown
		} else {
			0
		};
		for line in out_lines.iter().skip(start).take(shown) {
			lines.push(Line::from(Span::styled(format!("    {line}"), out_style)));
		}
		let remaining = out_lines.len() - shown;
		if remaining > 0 {
			lines.push(Line::from(Span::styled(
				format!("  … ({remaining} more lines, {expand_hint} to expand)"),
				Style::new().fg(theme.muted),
			)));
		}
	}
	lines.push(Line::default());
	lines
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// `key=value key=value` on one line, JSON-stringified values, cut to
/// COLLAPSED_ARGS_CHARS (render-utils.ts formatToolCallWithArgs).
fn compact_args_pairs(args: &serde_json::Value) -> String {
	let Some(obj) = args.as_object() else {
		if args.is_null() {
			return String::new();
		}
		return truncate_chars(&args.to_string(), COLLAPSED_ARGS_CHARS);
	};
	if obj.is_empty() {
		return String::new();
	}
	let pairs = obj
		.iter()
		.map(|(k, v)| format!("{k}={v}"))
		.collect::<Vec<_>>()
		.join(" ");
	truncate_chars(&pairs, COLLAPSED_ARGS_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
	if s.chars().count() <= max {
		return s.to_string();
	}
	let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
	out.push_str("...");
	out
}

/// Patch every span style of a rendered line (used for user bg and thinking
/// color/italic overlays, like pi's MarkdownTheme color functions).
fn patch_line(line: Line<'static>, f: impl Fn(Style) -> Style) -> Line<'static> {
	Line::from(
		line.spans
			.into_iter()
			.map(|span| Span::styled(span.content, f(span.style)))
			.collect::<Vec<_>>(),
	)
}

fn thinking_label(level: pi_ai::ThinkingLevel) -> String {
	match level {
		pi_ai::ThinkingLevel::Off => "off".into(),
		pi_ai::ThinkingLevel::Minimal => "minimal".into(),
		pi_ai::ThinkingLevel::Low => "low".into(),
		pi_ai::ThinkingLevel::Medium => "medium".into(),
		pi_ai::ThinkingLevel::High => "high".into(),
		pi_ai::ThinkingLevel::Xhigh => "xhigh".into(),
		pi_ai::ThinkingLevel::Max => "max".into(),
	}
}

fn truncate_to_width(text: &str, max: usize) -> String {
	if UnicodeWidthStr::width(text) <= max {
		return text.to_string();
	}
	let mut out = String::new();
	let mut w = 0usize;
	for c in text.chars() {
		let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
		if w + cw > max.saturating_sub(1) {
			break;
		}
		out.push(c);
		w += cw;
	}
	out.push('…');
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

#[cfg(test)]
mod tests {
	use super::*;
	use pi_tui::keybindings::KeybindingsManager;
	use pi_tui::parse_chord;

	#[test]
	fn tool_result_text_handles_partial_and_final_content() {
		assert_eq!(
			tool_result_text(&serde_json::json!({"content": [{"type":"text", "text":"running"}]})),
			"running"
		);
		assert_eq!(
			tool_result_text(&serde_json::json!({"content": ["done"]})),
			"done"
		);
	}

	fn key_event(chord: &str) -> KeyEvent {
		parse_chord(chord).unwrap().to_key_event()
	}

	#[test]
	fn token_formatting() {
		assert_eq!(format_tokens(999), "999");
		assert_eq!(format_tokens(1500), "1.5k");
		assert_eq!(format_tokens(45_000), "45k");
		assert_eq!(format_tokens(1_500_000), "1.5M");
		assert_eq!(format_tokens(45_000_000), "45M");
	}

	#[test]
	fn duration_formatting() {
		assert_eq!(format_duration(4200), "4.2s");
		assert_eq!(format_duration(90_000), "1m 30s");
		assert_eq!(format_duration(3_723_000), "1h 2m 3s");
	}

	#[test]
	fn footer_context_segment_formats_percent_and_unknown() {
		let theme = Theme::default();
		let (text, style) = footer_context_segment(Some((1000, 200_000)), 0, false, &theme);
		assert_eq!(text, "0.5%/200k");
		assert_eq!(style, Style::default());

		// Unknown usage (post-compaction): `?` in the percent slot, the window
		// still from the model, neutral color (footer.ts).
		let (text, style) = footer_context_segment(None, 200_000, false, &theme);
		assert_eq!(text, "?/200k");
		assert_eq!(style, Style::default());

		// Zero window falls back to the same `?/0` shape.
		let (text, _) = footer_context_segment(None, 0, false, &theme);
		assert_eq!(text, "?/0");
	}

	#[test]
	fn footer_context_segment_thresholds_and_auto_indicator() {
		let theme = Theme::default();
		let (text, style) = footer_context_segment(Some((150_000, 200_000)), 0, true, &theme);
		assert_eq!(text, "75.0%/200k (auto)");
		assert_eq!(style, Style::new().fg(theme.tool));

		let (text, style) = footer_context_segment(Some((190_000, 200_000)), 0, true, &theme);
		assert_eq!(text, "95.0%/200k (auto)");
		assert_eq!(style, Style::new().fg(theme.error));

		// Thresholds are strict (>70, >90): exactly 70% stays neutral, exactly
		// 90% falls into the warning band (footer.ts).
		let (_, style) = footer_context_segment(Some((140_000, 200_000)), 0, false, &theme);
		assert_eq!(style, Style::default());
		let (_, style) = footer_context_segment(Some((180_000, 200_000)), 0, false, &theme);
		assert_eq!(style, Style::new().fg(theme.tool));

		// Unknown usage is neutral even with auto-compaction enabled.
		let (_, style) = footer_context_segment(None, 200_000, true, &theme);
		assert_eq!(style, Style::default());
	}

	fn tool_block(id: &str, running: bool, output: &str) -> ChatBlock {
		ChatBlock::Tool {
			id: id.to_string(),
			name: "bash".to_string(),
			args: serde_json::json!({"command": "echo hi"}),
			running,
			is_error: false,
			output: output.to_string(),
			started: None,
			duration_ms: None,
			rendered: None,
		}
	}

	#[test]
	fn tool_update_appends_text_to_running_block() {
		let mut blocks = vec![tool_block("call-1", true, "")];
		assert!(apply_tool_update(
			&mut blocks,
			"call-1",
			&serde_json::json!({"text": "line1\n", "isError": false})
		));
		assert!(apply_tool_update(
			&mut blocks,
			"call-1",
			&serde_json::json!({"text": "line2\n"})
		));
		let ChatBlock::Tool { output, .. } = &blocks[0] else {
			panic!("expected tool block");
		};
		assert_eq!(output, "line1\nline2\n");

		// Carriage returns are dropped (progress-bar redraws).
		assert!(apply_tool_update(
			&mut blocks,
			"call-1",
			&serde_json::json!({"text": "50%\r"})
		));
		let ChatBlock::Tool { output, .. } = &blocks[0] else {
			panic!("expected tool block");
		};
		assert_eq!(output, "line1\nline2\n50%");
	}

	#[test]
	fn tool_update_ignores_finished_and_unknown_blocks() {
		let mut blocks = vec![
			tool_block("done", false, "final"),
			tool_block("running", true, "tail"),
		];
		// A late update after ToolExecutionEnd must not touch the block.
		assert!(!apply_tool_update(
			&mut blocks,
			"done",
			&serde_json::json!({"text": "x"})
		));
		// Unknown ids are ignored.
		assert!(!apply_tool_update(
			&mut blocks,
			"call-9",
			&serde_json::json!({"text": "x"})
		));
		// The matching running block still receives its own updates.
		assert!(apply_tool_update(
			&mut blocks,
			"running",
			&serde_json::json!({"text": "!"})
		));
		let ChatBlock::Tool { output, .. } = &blocks[1] else {
			panic!("expected tool block");
		};
		assert_eq!(output, "tail!");
		let ChatBlock::Tool { output, .. } = &blocks[0] else {
			panic!("expected tool block");
		};
		assert_eq!(output, "final");
	}

	#[test]
	fn tool_update_falls_back_to_content_payload() {
		let mut blocks = vec![tool_block("call-1", true, "")];
		assert!(apply_tool_update(
			&mut blocks,
			"call-1",
			&serde_json::json!({"content": [{"type": "text", "text": "chunk"}]})
		));
		let ChatBlock::Tool { output, .. } = &blocks[0] else {
			panic!("expected tool block");
		};
		assert_eq!(output, "chunk");
	}

	#[test]
	fn running_tool_preview_shows_output_tail() {
		let theme = Theme::default();
		let output = (0..30)
			.map(|i| format!("line{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		let lines = render_tool_block(
			"bash",
			&serde_json::json!({"command": "yes"}),
			true,
			false,
			&output,
			None,
			120,
			&theme,
			false,
			0,
			"ctrl+o",
		);
		let rendered: Vec<String> = lines
			.iter()
			.map(|l| {
				l.spans
					.iter()
					.map(|s| s.content.to_string())
					.collect::<String>()
			})
			.collect();
		// Tail preview: the last source line is visible, the first is not.
		assert!(rendered.iter().any(|l| l.contains("line29")));
		assert!(!rendered.iter().any(|l| l.contains("line0")));
		// Hidden head count with the expand hint.
		assert!(rendered.iter().any(|l| l.contains("10 more lines")));
	}

	#[test]
	fn args_pairs_collapsed() {
		let args = serde_json::json!({"path": "/tmp/a.txt", "limit": 5});
		let pairs = compact_args_pairs(&args);
		assert_eq!(pairs, "path=\"/tmp/a.txt\" limit=5");
	}

	#[test]
	fn args_pairs_truncated() {
		let mut obj = serde_json::Map::new();
		obj.insert("cmd".into(), serde_json::Value::String("x".repeat(300)));
		let pairs = compact_args_pairs(&serde_json::Value::Object(obj));
		assert!(pairs.chars().count() <= COLLAPSED_ARGS_CHARS);
		assert!(pairs.ends_with("..."));
	}

	#[test]
	fn global_actions_resolve_from_default_table() {
		// The manager interactive uses: TUI + app definitions merged.
		let keybindings = crate::core::ui_config::new_keybindings_manager();
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+c"), true),
			Some(GlobalAction::Clear)
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+d"), true),
			Some(GlobalAction::Exit)
		);
		// ctrl+d only exits with an empty editor; otherwise it falls through
		// to the editor (upstream app.exit behavior).
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+d"), false),
			None
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+o"), false),
			Some(GlobalAction::ToolsExpand)
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+t"), false),
			Some(GlobalAction::ThinkingToggle)
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+l"), false),
			Some(GlobalAction::ModelSelect)
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("escape"), false),
			Some(GlobalAction::Interrupt)
		);
		// Keys bound to undispatched actions stay undispatched (upstream
		// app.suspend / app.tree.filter.cycleForward share chords with
		// dispatched actions but must not fire here).
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+z"), false),
			None
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("x"), false),
			None
		);
	}

	#[test]
	fn custom_binding_replaces_the_default_key() {
		// keybindings.json `{"app.tools.expand": ["ctrl+y"]}`: the new chord
		// triggers tool expansion, the old default no longer does.
		let mut keybindings = crate::core::ui_config::new_keybindings_manager();
		keybindings
			.set_user_bindings(
				std::iter::once(("app.tools.expand".to_string(), vec!["ctrl+y".to_string()]))
					.collect(),
			)
			.unwrap();
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+y"), false),
			Some(GlobalAction::ToolsExpand)
		);
		// ctrl+o is still bound in the merged table
		// (app.tree.filter.cycleForward) but no longer to a dispatched action.
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+o"), false),
			None
		);
	}

	#[test]
	fn interrupt_takes_precedence_over_shadowing_tui_actions() {
		// The merged table binds escape to tui.select.cancel (and ctrl+d to
		// tui.editor.deleteCharForward); the app actions are checked first,
		// mirroring custom-editor.ts.
		let keybindings = crate::core::ui_config::new_keybindings_manager();
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("escape"), false),
			Some(GlobalAction::Interrupt)
		);
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+d"), true),
			Some(GlobalAction::Exit)
		);
	}

	#[test]
	fn disabled_action_falls_through() {
		// keybindings.json `{"app.tools.expand": []}` disables the action.
		let mut keybindings = KeybindingsManager::new();
		keybindings
			.set_user_bindings(std::iter::once(("app.tools.expand".to_string(), vec![])).collect())
			.unwrap();
		assert_eq!(
			resolve_global_action(&keybindings, &key_event("ctrl+o"), false),
			None
		);
	}
}
