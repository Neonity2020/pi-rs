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
//!
//! Session commands (`interactive-mode.ts:3030-3250`, `5496-5680`,
//! `6505-6620`): `/fork` opens the user-message selector and forks at the
//! message's parent (`runtimeHost.fork` → the round-nine replacement
//! machinery), `/clone` forks at the current leaf, `/tree` opens the
//! tree-selector port (`pi-tui` `TreeSelector`, filter modes from
//! `settings.treeFilterMode`), `/import` confirms and swaps to a copied JSONL
//! log, `/export` writes the raw branch as JSONL or renders HTML. Declared
//! deviations: the branch summary skips upstream's cumulative
//! read/modified-file tracking (the entry `details` stay empty), the
//! tree selector omits folding, label editing and clipboard copy, and
//! steering-queued messages cannot be restored to the editor on abort (only
//! prompts queued while busy come back via the navigation outcome).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthStr;

use pi_agent::AgentEvent;
use pi_ai::{Message, Model, StopReason};
use pi_tui::components::{
	Component, SelectAction, SelectItem, SelectList, TreeAction, TreeCategory, TreeFilterMode,
	TreeItem, TreeSelector, markdown_lines,
};
use pi_tui::keybindings::KeybindingsManager;
use pi_tui::{Editor, ScrollView, TerminalGuard, Theme};

use crate::core::agent_session::{
	RpcReplacement, SessionCommand, SessionEvent, SessionHandle, TreeNavResult, WireEvent,
};
use crate::core::model_registry::{ModelRegistry, add_provider};
use crate::core::session_manager::EntryKind;
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
#[derive(Debug)]
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
	kind: OverlayKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
	Model,
	Session,
	/// `/fork`: pick a user message to branch from.
	Fork,
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
			kind: OverlayKind::Model,
		}
	}

	fn sessions(sessions: Vec<std::path::PathBuf>, cancelled: impl Fn() -> bool) -> Self {
		let (items, search_texts): (Vec<_>, Vec<_>) = sessions
			.into_iter()
			.take_while(|_| !cancelled())
			.map(|path| {
				use crate::core::session_manager::{EntryKind, load_session_file};
				let (header, entries) = load_session_file(&path.join("session.jsonl"));
				let count = entries.len() + usize::from(header.is_some());
				let name = entries
					.iter()
					.rev()
					.find_map(|entry| match &entry.kind {
						EntryKind::SessionInfo { name } => {
							Some(name.as_deref().unwrap_or_default())
						}
						_ => None,
					})
					.filter(|name| !name.trim().is_empty());
				let first_message = entries
					.iter()
					.find_map(|entry| match &entry.kind {
						EntryKind::Message { message } => match message.as_ref() {
							Message::User(user) => Some(user.content.as_text()),
							_ => None,
						},
						_ => None,
					})
					.filter(|text| !text.trim().is_empty());
				let fallback = path
					.file_name()
					.map(|name| name.to_string_lossy().into_owned())
					.unwrap_or_else(|| path.display().to_string());
				let label = name
					.map(str::to_owned)
					.or_else(|| first_message.clone())
					.unwrap_or(fallback);
				let cwd = header
					.map(|header| header.cwd)
					.filter(|cwd| !cwd.is_empty())
					.unwrap_or_else(|| "unknown project".into());
				let description = match (name, first_message) {
					(Some(_), Some(message)) => format!("{cwd} · {count} entries · {message}"),
					_ => format!("{cwd} · {count} entries"),
				};
				let clean = |text: &str| {
					text.chars()
						.map(|c| if c.is_control() { ' ' } else { c })
						.take(200)
						.collect::<String>()
				};
				let messages = entries
					.iter()
					.filter_map(|entry| match &entry.kind {
						EntryKind::Message { message } => match message.as_ref() {
							Message::User(user) => Some(user.content.as_text()),
							Message::Assistant(assistant) => Some(assistant.text()),
							_ => None,
						},
						_ => None,
					})
					.collect::<Vec<_>>()
					.join(" ");
				let searchable = format!("{} {cwd} {messages}", name.unwrap_or_default());
				(
					SelectItem {
						label: clean(&label),
						value: path.to_string_lossy().into_owned(),
						description: Some(clean(&description)),
					},
					searchable,
				)
			})
			.unzip();
		let mut list = SelectList::new(items, 10);
		list.set_search_texts(search_texts);
		list.set_no_match_message("No matching sessions");
		Self {
			search: String::new(),
			list,
			kind: OverlayKind::Session,
		}
	}

	/// `/fork` selector — the user messages a fork can start from
	/// (`UserMessageSelectorComponent` upstream: entry id + text preview, the
	/// metadata line carries the position in history).
	fn fork(messages: Vec<(String, String)>) -> Self {
		let total = messages.len();
		let clean = |text: &str| {
			text.chars()
				.map(|c| if c.is_control() { ' ' } else { c })
				.take(200)
				.collect::<String>()
		};
		let items = messages
			.into_iter()
			.enumerate()
			.map(|(index, (id, text))| SelectItem {
				value: id,
				label: clean(&text),
				description: Some(format!("Message {} of {total}", index + 1)),
			})
			.collect();
		let mut list = SelectList::new(items, 10);
		list.set_no_match_message("No matching messages");
		Self {
			search: String::new(),
			list,
			kind: OverlayKind::Fork,
		}
	}
}

/// Interactive-only dialogs alongside the list overlay: `/import`
/// confirmation and the `/tree` branch-summary prompts (upstream
/// `showExtensionConfirm` / `showExtensionSelector` / `showExtensionEditor`).
#[derive(Clone)]
enum Dialog {
	/// `Replace current session with <path>?`
	ConfirmImport { path: String },
	/// `Summarize branch?` — No summary / Summarize / Summarize with custom
	/// prompt. Esc returns to the tree selector with the original selection.
	SummarizeChoice { target_id: String, cursor: usize },
	/// Custom summarization instructions (single-line editor; Esc returns to
	/// the choice).
	CustomPrompt { target_id: String, value: String },
}

impl Dialog {
	const SUMMARY_CHOICES: [&str; 3] = ["No summary", "Summarize", "Summarize with custom prompt"];
}

/// Which session-replacement command is in flight (`begin_replacement`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplacementKind {
	New,
	Switch,
	Fork,
	Clone,
	Import,
}

/// The response channel of a running `/tree` navigation plus the entry to
/// re-select when the branch summary is cancelled.
type PendingTreeNav = (
	tokio::sync::oneshot::Receiver<Result<TreeNavResult, String>>,
	Option<String>,
);

pub struct InteractiveMode {
	session: SessionHandle,
	models: Vec<Model>,
	registry: Arc<Mutex<ModelRegistry>>,
	// Session storage root (`--session-dir` override; `config::sessions_dir()`
	// by default). `/resume` and its selector resolve against it.
	session_root: PathBuf,
	provider_wizard: Option<ProviderWizard>,
	trust_prompt: Option<(std::path::PathBuf, std::path::PathBuf)>,
	pending_replacement: Option<tokio::sync::oneshot::Receiver<Result<Option<String>, String>>>,
	replacement_event_seen: bool,
	loading_sessions: Option<tokio::sync::oneshot::Receiver<ModelOverlay>>,
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
	// `/tree` selector overlay.
	tree: Option<TreeSelector>,
	// Interactive dialog (import confirm / summarize-branch prompts).
	dialog: Option<Dialog>,
	// Pending `/tree` navigation result plus the entry to re-select when the
	// branch summary is cancelled (Esc re-opens the tree at the same node).
	pending_tree: Option<PendingTreeNav>,
	// Session-replacement bookkeeping: which command is in flight, the error
	// prefix and the success status its completion reports.
	replacement_kind: ReplacementKind,
	replacement_error_prefix: String,
	replacement_status: Option<String>,
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
		Self::with_session_root(
			session,
			models,
			registry,
			cwd,
			ui,
			crate::config::sessions_dir(),
		)
	}

	/// `new` with an explicit session storage root (`--session-dir` /
	/// `PI_SESSION_DIR` / settings `sessionDir`). `/resume` and its selector
	/// resolve against this root instead of the default sessions dir.
	pub fn with_session_root(
		session: SessionHandle,
		models: Vec<Model>,
		registry: Arc<Mutex<ModelRegistry>>,
		cwd: String,
		ui: UiConfig,
		session_root: PathBuf,
	) -> Self {
		let (ui_tx, ui_rx) = mpsc::unbounded_channel();
		let theme = ui.theme;
		let mut mode = Self {
			session,
			models,
			registry,
			session_root,
			provider_wizard: None,
			trust_prompt: None,
			pending_replacement: None,
			replacement_event_seen: false,
			loading_sessions: None,
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
			tree: None,
			dialog: None,
			pending_tree: None,
			replacement_kind: ReplacementKind::Switch,
			replacement_error_prefix: "Session switch failed".into(),
			replacement_status: None,
			ui_rx,
			ui_tx,
		};
		mode.restore_transcript();
		mode
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
				result = async {
					match self.pending_replacement.as_mut() {
						Some(receiver) => receiver.await,
						None => std::future::pending().await,
					}
				} => {
					match result {
						Ok(outcome) => self.complete_replacement(outcome),
						Err(_) => {
							self.push_notice(format!("{}: session task stopped", self.replacement_error_prefix));
						}
					}
					self.pending_replacement = None;
					self.replacement_event_seen = false;
					self.replacement_status = None;
					self.replacement_kind = ReplacementKind::Switch;
				}
				// `/tree` branch navigation: the response resolves once the
				// (possibly summarized) navigation lands.
				result = async {
					match self.pending_tree.as_mut() {
						Some((receiver, _)) => receiver.await,
						None => std::future::pending().await,
					}
				} => {
					let pending = self.pending_tree.take();
					match result {
						Ok(Ok(TreeNavResult::Navigated { editor_text, queued_text })) => {
							// Queued prompts return to the editor ahead of its
								// current text (upstream
								// `restoreQueuedMessagesToEditor`); the target user
								// message pre-fills only an empty editor.
								let mut fill = String::new();
								if let Some(queued) = queued_text {
									fill.push_str(&queued);
								}
								let current = self.editor.text().trim().to_string();
								if !current.is_empty() {
									if !fill.is_empty() {
										fill.push_str("\n\n");
									}
									fill.push_str(&current);
								} else if fill.is_empty() && let Some(text) = editor_text {
									fill.push_str(&text);
								}
								self.editor.set_text(&fill);
								self.push_notice("Navigated to selected point".into());
							}
						Ok(Ok(TreeNavResult::Aborted)) => {
							self.push_notice("Branch summarization cancelled".into());
							if let Some((_, Some(target))) = pending {
								self.show_tree_selector(Some(&target));
							}
						}
						Ok(Err(error)) => self.push_notice(error),
						Err(_) => self.push_notice("navigation failed: session task stopped".into()),
					}
				}
				result = async {
					match self.loading_sessions.as_mut() {
						Some(receiver) => receiver.await,
						None => std::future::pending().await,
					}
				} => {
					self.loading_sessions = None;
					match result {
						Ok(overlay) if overlay.list.is_empty() => self.push_notice("no saved sessions yet".into()),
						Ok(overlay) => self.overlay = Some(overlay),
						Err(_) => self.push_notice("cannot load saved sessions".into()),
					}
				}
				event = term_rx.recv() => {
					match event {
						Some(event) => self.handle_terminal_event(event),
						None => self.quit = true,
					}
				}
				event = self.session.wire_events.recv() => {
					if let Ok(WireEvent::SessionInfoChanged { name }) = event {
						self.push_notice(format!("Session name: {}", name.as_deref().unwrap_or("(none)")));
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
		if self.loading_sessions.is_some()
			&& let TermEvent::Key(key) = &event
		{
			match key.code {
				KeyCode::Esc => {
					self.loading_sessions = None;
					self.push_notice("Session list loading cancelled".into());
					return;
				}
				KeyCode::Enter => {
					self.push_notice("Loading sessions; press Enter again when it finishes".into());
					return;
				}
				_ => {}
			}
			if let Some(action) =
				resolve_global_action(&self.keybindings, key, self.editor.is_empty())
			{
				if matches!(action, GlobalAction::Exit | GlobalAction::Clear) {
					self.quit = true;
				}
				return;
			}
		}
		if self.pending_replacement.is_some()
			&& let TermEvent::Key(key) = &event
		{
			if key.code == KeyCode::Enter {
				self.push_notice(
					"Session switch in progress; press Enter again when it finishes".into(),
				);
				return;
			}
			if self.keybindings.matches("app.clear", key)
				|| (self.editor.is_empty() && self.keybindings.matches("app.exit", key))
			{
				self.quit = true;
				return;
			}
			if resolve_global_action(&self.keybindings, key, self.editor.is_empty()).is_some() {
				return;
			}
		}
		if self.trust_prompt.is_some() {
			if let TermEvent::Key(key) = event
				&& key.kind != crossterm::event::KeyEventKind::Release
			{
				self.handle_trust_key(key);
			}
			return;
		}
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
			if self.dialog.is_some() {
				self.handle_dialog_key(*key);
				return;
			}
			if self.tree.is_some() {
				self.handle_tree_key(*key);
				return;
			}
			// While a branch summary runs, escape aborts it
			// (`abortBranchSummary` upstream; the tree selector re-opens at the
			// original selection when the summary cancels).
			if self.pending_tree.is_some() && self.keybindings.matches("app.interrupt", key) {
				let _ = self.session.cmd_tx.send(SessionCommand::AbortBranchSummary);
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

	fn handle_trust_key(&mut self, key: KeyEvent) {
		if key
			.modifiers
			.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
		{
			return;
		}
		match key.code {
			KeyCode::Char('y' | 'Y') => {
				let (path, cwd) = self.trust_prompt.take().unwrap();
				match crate::core::trust::trust(&cwd) {
					Ok(()) => self.switch_session(path),
					Err(error) => self.push_notice(format!("cannot save project trust: {error}")),
				}
			}
			KeyCode::Char('n' | 'N') => {
				let (path, _) = self.trust_prompt.take().unwrap();
				self.switch_session(path);
			}
			KeyCode::Esc => {
				self.trust_prompt = None;
				self.push_notice("Session switch cancelled".into());
			}
			_ => {}
		}
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
							registry.reload(Some(&default.key()));
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
				let selection = match self.overlay.as_ref() {
					Some(overlay) => overlay
						.list
						.filtered()
						.get(index)
						.map(|item| (overlay.kind, item.value.clone())),
					None => None,
				};
				if let Some((kind, value)) = selection {
					self.overlay = None;
					match kind {
						OverlayKind::Model => {
							let _ = self
								.session
								.cmd_tx
								.send(SessionCommand::SetModel { query: value });
						}
						OverlayKind::Session => self.resume_session(&value),
						OverlayKind::Fork => {
							self.begin_replacement_labeled(
								RpcReplacement::Fork { entry_id: value },
								ReplacementKind::Fork,
								"Session switch failed",
								None,
							);
						}
					}
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
			"new" => {
				self.begin_replacement_labeled(
					RpcReplacement::New {
						parent_session: None,
					},
					ReplacementKind::New,
					"Session switch failed",
					None,
				);
			}
			"resume" => {
				if args.is_empty() {
					let (sender, receiver) = tokio::sync::oneshot::channel();
					self.loading_sessions = Some(receiver);
					let session_root = self.session_root.clone();
					std::thread::spawn(move || {
						let sessions =
							crate::core::SessionManager::recent_paths_in(&session_root, usize::MAX);
						let overlay = ModelOverlay::sessions(sessions, || sender.is_closed());
						let _ = sender.send(overlay);
					});
				} else {
					self.resume_session(args);
				}
			}
			"fork" => {
				let messages = self.fork_messages();
				if messages.is_empty() {
					self.push_notice("No messages to fork from".into());
				} else {
					self.overlay = Some(ModelOverlay::fork(messages));
				}
			}
			"clone" => {
				let has_leaf = self.session.log.lock().unwrap().session.leaf_id().is_some();
				if !has_leaf {
					self.push_notice("Nothing to clone yet".into());
				} else {
					self.begin_replacement_labeled(
						RpcReplacement::Clone,
						ReplacementKind::Clone,
						"Session switch failed",
						Some("Cloned to new session".into()),
					);
				}
			}
			"tree" => {
				self.show_tree_selector(None);
			}
			"export" => {
				let line = format!("/export {args}");
				let output = crate::core::slash_commands::path_command_argument(&line, "/export");
				match self.export_session(output) {
					Ok(path) => {
						self.push_notice(format!("Session exported to: {}", path.display()))
					}
					Err(error) => self.push_notice(format!("Failed to export session: {error}")),
				}
			}
			"import" => {
				let line = format!("/import {args}");
				match crate::core::slash_commands::path_command_argument(&line, "/import") {
					None => self.push_notice("Usage: /import <path.jsonl>".into()),
					Some(path) => {
						self.dialog = Some(Dialog::ConfirmImport {
							path: path.to_string(),
						});
					}
				}
			}
			"name" => {
				if args.is_empty() {
					self.push_notice("Usage: /name <name>".into());
				} else {
					let _ = self.session.cmd_tx.send(SessionCommand::SetSessionName {
						name: args.to_string(),
					});
				}
			}
			"session" => {
				let details = {
					let guard = self.session.log.lock().unwrap();
					let session = &guard.session;
					let id = session.session_id().unwrap_or_else(|| "(in-memory)".into());
					let path = session
						.session_dir()
						.map(|dir| dir.join("session.jsonl").display().to_string())
						.unwrap_or_else(|| "(not saved)".into());
					let name = session.session_name().unwrap_or_else(|| "(unnamed)".into());
					format!("session: {name}\nid: {id}\nfile: {path}\ncwd: {}", self.cwd)
				};
				self.push_notice(details);
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

	fn resume_session(&mut self, selector: &str) {
		use crate::core::session_manager::{
			SessionSelector, resolve_session_in, validate_session_dir,
		};
		let result = resolve_session_in(
			&self.session_root,
			std::path::Path::new(&self.cwd),
			&SessionSelector::ByIdOrPath(selector.to_string()),
		)
		.and_then(|path| {
			validate_session_dir(&path)?;
			Ok(path)
		});
		match result {
			Ok(path) => {
				let cwd = crate::core::session_manager::peek_session_header(&path)
					.map(|header| std::path::PathBuf::from(header.cwd));
				if let Some(cwd) =
					cwd.filter(|cwd| cwd.is_dir() && cwd.join(".pi/mcp.json").is_file())
					&& self.session.mcp_enabled()
					&& !crate::core::trust::is_trusted(&cwd)
				{
					self.trust_prompt = Some((path, cwd));
				} else {
					self.switch_session(path);
				}
			}
			Err(error) => self.push_notice(format!("cannot resume session: {error}")),
		}
	}

	fn switch_session(&mut self, path: std::path::PathBuf) {
		self.begin_replacement(RpcReplacement::Switch {
			path: path.to_string_lossy().into_owned(),
		});
	}

	/// User messages a `/fork` can start from — `getUserMessagesForForking`
	/// upstream: entry id + text, skipping empty texts.
	fn fork_messages(&self) -> Vec<(String, String)> {
		self.session
			.log
			.lock()
			.unwrap()
			.session
			.entries()
			.iter()
			.filter_map(|entry| {
				let id = entry.id()?;
				match &entry.kind {
					EntryKind::Message { message } => match message.as_ref() {
						Message::User(user) => {
							let text = user.content.as_text();
							(!text.trim().is_empty()).then(|| (id.to_string(), text))
						}
						_ => None,
					},
					_ => None,
				}
			})
			.collect()
	}

	/// Open the `/tree` selector (upstream `showTreeSelector`). Returns the
	/// refusal text when there is nothing to show or a navigation is already
	/// in flight.
	fn show_tree_selector(&mut self, initial_selected: Option<&str>) {
		if self.pending_tree.is_some() {
			self.push_notice(
				"Wait for the current compaction or tree navigation to finish before navigating the session tree."
					.into(),
			);
			return;
		}
		let (items, leaf) = self.tree_items();
		if items.is_empty() {
			self.push_notice("No entries in session".into());
			return;
		}
		let mode =
			TreeFilterMode::parse(crate::core::settings_manager::normalize_tree_filter_mode(
				crate::core::settings_manager::SettingsManager::load(Some(std::path::Path::new(
					&self.cwd,
				)))
				.effective()
				.tree_filter_mode
				.as_deref(),
			));
		self.tree = Some(TreeSelector::new(
			items,
			leaf.as_deref(),
			initial_selected,
			mode,
		));
	}

	/// Map the live session tree onto the neutral [`TreeItem`] rows
	/// (`getEntryDisplayText` upstream), resolving labels and tool calls.
	fn tree_items(&self) -> (Vec<TreeItem>, Option<String>) {
		let guard = self.session.log.lock().unwrap();
		let session = &guard.session;
		let mut labels: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
		for entry in session.entries() {
			if let EntryKind::Label {
				target_id,
				label: Some(label),
			} = &entry.kind
			{
				labels.insert(target_id.as_str(), label.as_str());
			}
		}
		// Assistant tool calls feed the tool-result rows (`toolCallMap`).
		let mut tool_calls: std::collections::HashMap<&str, (&str, &serde_json::Value)> =
			std::collections::HashMap::new();
		for entry in session.entries() {
			if let EntryKind::Message { message } = &entry.kind
				&& let Message::Assistant(assistant) = message.as_ref()
			{
				for call in assistant.tool_calls() {
					tool_calls.insert(call.id.as_str(), (call.name.as_str(), &call.arguments));
				}
			}
		}
		let normalize = |text: &str| {
			let line: String = text
				.chars()
				.map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
				.collect();
			let line = line.trim();
			line.chars().take(200).collect::<String>()
		};
		let items = session
			.entries()
			.iter()
			.filter_map(|entry| {
				let id = entry.id()?;
				let (category, prefix, text) = match &entry.kind {
					EntryKind::Message { message } => match message.as_ref() {
						Message::User(user) => (
							TreeCategory::UserMessage,
							"user: ".to_string(),
							normalize(&user.content.as_text()),
						),
						Message::Assistant(assistant) => {
							let text = assistant.text();
							let notable = !matches!(
								assistant.stop_reason,
								StopReason::Stop | StopReason::ToolUse
							);
							let display = if !text.trim().is_empty() {
								normalize(&text)
							} else if assistant.stop_reason == StopReason::Aborted {
								"(aborted)".to_string()
							} else if let Some(error) = &assistant.error_message {
								normalize(error).chars().take(80).collect()
							} else {
								"(no content)".to_string()
							};
							(
								TreeCategory::AssistantMessage {
									has_text: !text.trim().is_empty(),
									notable,
								},
								"assistant: ".to_string(),
								display,
							)
						}
						Message::ToolResult(result) => {
							let text = match tool_calls.get(result.tool_call_id.as_str()) {
								Some((name, args)) => {
									let pairs = compact_args_pairs(args);
									let pairs = if pairs.chars().count() > 50 {
										format!("{}…", pairs.chars().take(50).collect::<String>())
									} else {
										pairs
									};
									format!("[{name}: {pairs}]")
								}
								None => format!("[{}]", result.tool_name),
							};
							(TreeCategory::ToolResult, String::new(), text)
						}
						Message::System(_) => (TreeCategory::Hidden, String::new(), String::new()),
					},
					EntryKind::BashExecution { command, .. } => (
						TreeCategory::BashExecution,
						"[bash]: ".to_string(),
						normalize(command),
					),
					EntryKind::Compaction { tokens_before, .. } => (
						TreeCategory::Compaction,
						String::new(),
						format!("[compaction: {}k tokens]", tokens_before / 1000),
					),
					EntryKind::BranchSummary { summary, .. } => (
						TreeCategory::BranchSummary,
						"[branch summary]: ".to_string(),
						normalize(summary),
					),
					EntryKind::ModelChange { model_id, .. } => (
						TreeCategory::Settings,
						String::new(),
						format!("[model: {model_id}]"),
					),
					EntryKind::ThinkingLevelChange { thinking_level } => (
						TreeCategory::Settings,
						String::new(),
						format!("[thinking: {thinking_level}]"),
					),
					EntryKind::Custom { custom_type, .. } => (
						TreeCategory::Settings,
						String::new(),
						format!("[custom: {custom_type}]"),
					),
					EntryKind::ContextEdit {
						target_id,
						replacement,
					} => (
						TreeCategory::Settings,
						String::new(),
						format!(
							"[context {}: {target_id}]",
							if replacement.is_none() {
								"omit"
							} else {
								"replace"
							}
						),
					),
					EntryKind::Label { label, .. } => (
						TreeCategory::Settings,
						String::new(),
						format!("[label: {}]", label.as_deref().unwrap_or("(cleared)")),
					),
					EntryKind::SessionInfo { name } => (
						TreeCategory::Settings,
						String::new(),
						format!("[title: {}]", name.as_deref().unwrap_or("")),
					),
					EntryKind::CustomMessage {
						custom_type,
						content,
						..
					} => (
						TreeCategory::Always,
						format!("[{custom_type}]: "),
						normalize(&match content {
							serde_json::Value::String(text) => text.clone(),
							serde_json::Value::Array(blocks) => blocks
								.iter()
								.filter_map(|block| {
									block.get("text").and_then(serde_json::Value::as_str)
								})
								.collect::<Vec<_>>()
								.join(" "),
							_ => String::new(),
						}),
					),
					// `usage` and unknown entries stay invisible (upstream filters
					// usage; pi-rs `Unknown` preserves extension payload that has
					// no display text).
					EntryKind::Usage { .. } | EntryKind::Unknown(_) => {
						return None;
					}
				};
				Some(TreeItem {
					id: id.to_string(),
					parent_id: entry.parent_id().map(str::to_owned),
					category,
					badge: labels.get(id).map(|label| (*label).to_string()),
					prefix,
					text,
					search_extra: String::new(),
				})
			})
			.collect();
		let leaf = session.leaf_id().map(str::to_owned);
		(items, leaf)
	}

	fn handle_tree_key(&mut self, key: KeyEvent) {
		let Some(tree) = self.tree.as_mut() else {
			return;
		};
		match tree.handle_key(&key, &self.keybindings) {
			TreeAction::Select(id) => {
				let leaf = self
					.session
					.log
					.lock()
					.unwrap()
					.session
					.leaf_id()
					.map(str::to_owned);
				self.tree = None;
				if leaf.as_deref() == Some(id.as_str()) {
					self.push_notice("Already at this point".into());
					return;
				}
				// `branchSummary.skipPrompt` navigates without the three-way
				// prompt; otherwise ask first (`showExtensionSelector` upstream).
				let skip_prompt = crate::core::settings_manager::SettingsManager::load(Some(
					std::path::Path::new(&self.cwd),
				))
				.effective()
				.branch_summary
				.and_then(|settings| settings.skip_prompt)
				.unwrap_or(false);
				if skip_prompt {
					self.start_tree_navigation(id, false, None);
				} else {
					self.dialog = Some(Dialog::SummarizeChoice {
						target_id: id,
						cursor: 0,
					});
				}
			}
			TreeAction::Cancel => self.tree = None,
			TreeAction::Handled | TreeAction::Ignored => {}
		}
	}

	/// Commit to a tree navigation: the session task aborts an active run,
	/// optionally summarizes the abandoned branch, then moves the leaf and
	/// swaps the runtime (upstream `navigateTree`).
	fn start_tree_navigation(
		&mut self,
		target_id: String,
		summarize: bool,
		custom_instructions: Option<String>,
	) {
		let (response, received) = tokio::sync::oneshot::channel();
		if self
			.session
			.cmd_tx
			.send(SessionCommand::NavigateTree {
				target_id: target_id.clone(),
				summarize,
				custom_instructions,
				response,
			})
			.is_ok()
		{
			self.pending_tree = Some((received, Some(target_id)));
			self.dialog = None;
		} else {
			self.push_notice("navigation failed: session task stopped".into());
		}
	}

	fn handle_dialog_key(&mut self, key: KeyEvent) {
		let Some(dialog) = self.dialog.clone() else {
			return;
		};
		match dialog {
			Dialog::ConfirmImport { path } => match key.code {
				KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
					self.dialog = None;
					self.begin_replacement_labeled(
						RpcReplacement::Import { path: path.clone() },
						ReplacementKind::Import,
						"Failed to import session",
						Some(format!("Session imported from: {path}")),
					);
				}
				KeyCode::Char('n' | 'N') | KeyCode::Esc => {
					self.dialog = None;
					self.push_notice("Import cancelled".into());
				}
				_ => {}
			},
			Dialog::SummarizeChoice {
				target_id,
				mut cursor,
			} => match key.code {
				KeyCode::Up => {
					cursor = cursor.saturating_sub(1);
					self.dialog = Some(Dialog::SummarizeChoice { target_id, cursor });
				}
				KeyCode::Down => {
					cursor = (cursor + 1).min(Dialog::SUMMARY_CHOICES.len() - 1);
					self.dialog = Some(Dialog::SummarizeChoice { target_id, cursor });
				}
				KeyCode::Enter => {
					self.dialog = None;
					match cursor {
						0 => self.start_tree_navigation(target_id, false, None),
						1 => self.start_tree_navigation(target_id, true, None),
						_ => {
							self.dialog = Some(Dialog::CustomPrompt {
								target_id,
								value: String::new(),
							});
						}
					}
				}
				// Esc re-opens the tree selector with the original selection
				// (upstream re-shows it when the summary prompt cancels).
				KeyCode::Esc => {
					self.dialog = None;
					self.show_tree_selector(Some(&target_id));
				}
				_ => {}
			},
			Dialog::CustomPrompt {
				target_id,
				mut value,
			} => match key.code {
				KeyCode::Esc => {
					// Cancel loops back to the summary choice (upstream
					// `continue`s the prompt loop).
					self.dialog = Some(Dialog::SummarizeChoice {
						target_id,
						cursor: 0,
					});
				}
				KeyCode::Enter => {
					self.dialog = None;
					let custom = (!value.trim().is_empty()).then(|| value.trim().to_string());
					self.start_tree_navigation(target_id, true, custom);
				}
				KeyCode::Backspace => {
					value.pop();
					self.dialog = Some(Dialog::CustomPrompt { target_id, value });
				}
				KeyCode::Char(c)
					if !key
						.modifiers
						.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
				{
					value.push(c);
					self.dialog = Some(Dialog::CustomPrompt { target_id, value });
				}
				_ => {}
			},
		}
	}

	/// `/export`: `.jsonl` paths write the raw branch log
	/// (`exportSessionToJsonl` upstream), everything else renders HTML
	/// (`exportToHtml`; the port's exporter carries its own default theme).
	fn export_session(&self, output: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
		if let Some(path) = output.filter(|path| path.ends_with(".jsonl")) {
			self.export_jsonl(path)
		} else {
			let dir = self
				.session
				.log
				.lock()
				.unwrap()
				.session
				.session_dir()
				.map(Path::to_path_buf)
				.ok_or_else(|| anyhow::anyhow!("Cannot export in-memory session to HTML"))?;
			crate::core::export_html::write_session_html(&dir, output.map(std::path::Path::new))
		}
	}

	/// Serialize the current branch as JSONL — `serializeSessionBranch`
	/// upstream: a fresh header line followed by the leaf path with
	/// re-chained `parentId`s. Relative paths resolve against the process
	/// cwd; the default name is `session-<timestamp>.jsonl`.
	fn export_jsonl(&self, output: &str) -> anyhow::Result<std::path::PathBuf> {
		let resolved = if let Some(rest) = output.strip_prefix("~/") {
			std::env::var_os("HOME")
				.map(|home| std::path::PathBuf::from(home).join(rest))
				.unwrap_or_else(|| std::path::PathBuf::from(output))
		} else {
			std::path::PathBuf::from(output)
		};
		let resolved = if resolved.is_absolute() {
			resolved
		} else {
			std::env::current_dir()?.join(resolved)
		};
		let guard = self.session.log.lock().unwrap();
		let session = &guard.session;
		let header =
			session
				.header()
				.cloned()
				.unwrap_or(crate::core::session_manager::SessionHeader {
					version: Some(crate::core::session_manager::CURRENT_SESSION_VERSION),
					id: uuid::Uuid::now_v7().to_string(),
					timestamp: crate::core::session_manager::iso_timestamp(pi_ai::types::now_ms()),
					cwd: self.cwd.clone(),
					parent_session: None,
				});
		let mut parent = serde_json::Value::Null;
		let mut lines = String::new();
		let mut header_value = header.to_value();
		header_value["timestamp"] = serde_json::Value::String(
			crate::core::session_manager::iso_timestamp(pi_ai::types::now_ms()),
		);
		lines.push_str(&header_value.to_string());
		lines.push('\n');
		for entry in
			crate::core::session_manager::build_session_path(session.entries(), session.leaf_id())
		{
			let mut value = entry.to_value();
			value["parentId"] = parent.clone();
			parent = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
			lines.push_str(&value.to_string());
			lines.push('\n');
		}
		if let Some(dir) = resolved.parent() {
			std::fs::create_dir_all(dir)?;
		}
		std::fs::write(&resolved, lines)?;
		Ok(resolved)
	}

	/// Drain the replacement's trailing events and report its outcome. Split
	/// from the run-loop arm so tests can drive it through the response
	/// channel directly.
	fn complete_replacement(&mut self, outcome: Result<Option<String>, String>) {
		let kind = self.replacement_kind;
		match outcome {
			Ok(payload) => {
				while !self.replacement_event_seen {
					match self.session.events.try_recv() {
						Ok(event) => self.handle_session_event(event),
						Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
							self.push_notice(format!("event lag: dropped {count} events"))
						}
						Err(_) => break,
					}
				}
				if !self.replacement_event_seen {
					self.handle_session_event(SessionEvent::SessionReplaced);
				}
				match kind {
					// The message text of the forked-from entry comes back as
					// `selectedText` (upstream pre-fills the editor with it).
					ReplacementKind::Fork => {
						self.editor.set_text(payload.unwrap_or_default());
						self.push_notice("Forked to new session".into());
					}
					ReplacementKind::Clone => {
						self.editor.set_text("");
						self.push_notice("Cloned to new session".into());
					}
					ReplacementKind::Import => {
						if let Some(status) = self.replacement_status.take() {
							self.push_notice(status);
						}
					}
					ReplacementKind::New | ReplacementKind::Switch => {}
				}
			}
			Err(error) => {
				self.push_notice(format!("{}: {error}", self.replacement_error_prefix));
			}
		}
	}

	fn begin_replacement(&mut self, operation: RpcReplacement) {
		self.begin_replacement_labeled(
			operation,
			ReplacementKind::Switch,
			"Session switch failed",
			None,
		);
	}

	/// [`Self::begin_replacement`] with the completion bookkeeping: which
	/// command runs, the error prefix its failures carry and the status shown
	/// on success (`Forked to new session` / `Cloned to new session` /
	/// `Session imported from: …` upstream).
	fn begin_replacement_labeled(
		&mut self,
		operation: RpcReplacement,
		kind: ReplacementKind,
		error_prefix: &str,
		status: Option<String>,
	) {
		let (response, received) = tokio::sync::oneshot::channel();
		if self
			.session
			.cmd_tx
			.send(SessionCommand::RpcReplacement {
				operation,
				response,
			})
			.is_ok()
		{
			self.pending_replacement = Some(received);
			self.replacement_event_seen = false;
			self.replacement_kind = kind;
			self.replacement_error_prefix = error_prefix.to_string();
			self.replacement_status = status;
		} else {
			self.push_notice(format!("{error_prefix}: session task stopped"));
		}
	}

	fn restore_transcript(&mut self) {
		self.blocks.clear();
		self.chat.clear();
		self.usage = (0, 0, 0, 0, 0.0);
		for message in self.session.agent.snapshot().messages {
			match &message {
				Message::User(_) => self.handle_agent_event(AgentEvent::MessageStart { message }),
				Message::Assistant(assistant) => {
					let calls: Vec<_> = assistant
						.content
						.iter()
						.filter_map(|content| match content {
							pi_ai::AssistantContent::ToolCall(call) => Some(call.clone()),
							_ => None,
						})
						.collect();
					self.handle_agent_event(AgentEvent::MessageStart {
						message: message.clone(),
					});
					self.handle_agent_event(AgentEvent::MessageEnd { message });
					for call in calls {
						self.handle_agent_event(AgentEvent::ToolExecutionStart {
							tool_call_id: call.id,
							tool_name: call.name,
							args: call.arguments,
						});
						if let Some(ChatBlock::Tool { started, .. }) = self.blocks.last_mut() {
							*started = None;
						}
					}
				}
				Message::ToolResult(result) => {
					if !self.blocks.iter().any(
						|block| matches!(block, ChatBlock::Tool { id, running: true, .. } if id == &result.tool_call_id),
					) {
						self.handle_agent_event(AgentEvent::ToolExecutionStart {
							tool_call_id: result.tool_call_id.clone(),
							tool_name: result.tool_name.clone(),
							args: serde_json::json!({}),
						});
						if let Some(ChatBlock::Tool { started, .. }) = self.blocks.last_mut() {
							*started = None;
						}
					}
					self.handle_agent_event(AgentEvent::ToolExecutionEnd {
						tool_call_id: result.tool_call_id.clone(),
						tool_name: result.tool_name.clone(),
						result: serde_json::to_value(result).unwrap_or_default(),
						is_error: result.is_error,
						duration_ms: result.duration_ms,
					});
				}
				Message::System(_) => {}
			}
		}
		for block in &mut self.blocks {
			if let ChatBlock::Tool {
				running,
				is_error,
				output,
				..
			} = block && *running
			{
				*running = false;
				*is_error = true;
				*output = "Tool result is missing from the session".into();
			}
		}
	}

	// -- agent events -----------------------------------------------------------

	fn handle_session_event(&mut self, event: SessionEvent) {
		match event {
			SessionEvent::Agent(event) => self.handle_agent_event(*event),
			SessionEvent::Notice(text) => self.push_notice(text),
			SessionEvent::SessionReplaced => {
				if self.pending_replacement.is_some() {
					self.replacement_event_seen = true;
				}
				self.session.agent = self.session.shared.live_agent.lock().unwrap().clone();
				if let Some(header) = self.session.log.lock().unwrap().session.header() {
					self.cwd = header.cwd.clone();
				}
				self.restore_transcript();
				self.refresh_branch();
				self.working = false;
			}
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
				duration_ms: event_duration,
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
					*duration_ms = event_duration
						.map(u128::from)
						.or_else(|| started.map(|s| s.elapsed().as_millis()));
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
		let cwd = self.cwd.clone();
		tokio::spawn(async move {
			let output = tokio::process::Command::new("git")
				.args(["rev-parse", "--abbrev-ref", "HEAD"])
				.current_dir(cwd)
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
		self.draw_tree(frame, chunks[1]);
		self.draw_dialog(frame, chunks[1]);
		if let Some((_, cwd)) = &self.trust_prompt {
			let area = chunks[1];
			let panel = Rect {
				height: area.height.min(8),
				..area
			};
			frame.render_widget(Clear, panel);
			let text = format!(
				"MCP configuration can run commands. Trust this project?\nY: trust and resume · N: skip project MCP · Esc: cancel\nProject: {}",
				cwd.display()
			);
			frame.render_widget(
				Paragraph::new(text)
					.wrap(Wrap { trim: false })
					.block(Block::bordered().title(" Project trust ")),
				panel,
			);
		}
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
				if self.loading_sessions.is_some() {
					" loading sessions (esc cancels) "
				} else if self.pending_tree.is_some() {
					" summarizing branch (esc aborts) "
				} else if self.pending_replacement.is_some() {
					match self.replacement_kind {
						ReplacementKind::Import => " importing session… ",
						ReplacementKind::New => " starting new session… ",
						_ => " switching session… ",
					}
				} else if self.working {
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
		let width = chat_area.width.min(72);
		let height = chat_area.height.min(16);
		if width < 4 || height < 6 {
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
				match overlay.kind {
					OverlayKind::Model => " Select model ",
					OverlayKind::Session => " Resume session ",
					OverlayKind::Fork => " Fork from message ",
				},
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
		let list_limit = (inner.height as usize).saturating_sub(4).max(1);
		let mut list_lines =
			overlay
				.list
				.render_lines_with_limit(inner.width as usize, &self.theme, list_limit);
		list_lines.truncate((inner.height as usize).saturating_sub(3));
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

	/// The `/tree` selector panel (upstream `TreeSelectorComponent`: bordered
	/// header, search line, tree rows, counter).
	fn draw_tree(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(tree) = self.tree.as_ref() else {
			return;
		};
		let width = chat_area
			.width
			.saturating_sub(4)
			.max(30)
			.min(chat_area.width);
		let height = chat_area
			.height
			.saturating_sub(2)
			.max(10)
			.min(chat_area.height);
		let area = Rect {
			x: chat_area.x + (chat_area.width - width) / 2,
			y: chat_area.y + 1,
			width,
			height,
		};
		frame.render_widget(Clear, area);
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.accent))
			.title(Span::styled(
				" Session tree ",
				Style::new()
					.fg(self.theme.accent)
					.add_modifier(Modifier::BOLD),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);

		let search_line = Line::from(vec![
			Span::styled("Type to search: ", Style::new().fg(self.theme.muted)),
			Span::styled(
				tree.search().to_string(),
				Style::new().fg(self.theme.accent),
			),
		]);
		let max_visible = (inner.height as usize).saturating_sub(4).max(1);
		let mut lines = tree.render_lines(inner.width as usize, &self.theme, max_visible);
		lines.truncate((inner.height as usize).saturating_sub(2));
		let mut rows = vec![search_line];
		rows.extend(lines);
		rows.push(Line::from(Span::styled(
			format!(
				" {} select · {} cancel · type to search · ctrl+o filter",
				self.keybinding_hint("tui.select.confirm", "enter"),
				self.keybinding_hint("tui.select.cancel", "esc"),
			),
			Style::new().fg(self.theme.muted),
		)));
		rows.resize(inner.height as usize, Line::default());
		frame.render_widget(Paragraph::new(rows), inner);
	}

	/// Centered dialog panels: `/import` confirmation, the summarize-branch
	/// three-way choice, and the custom-prompt editor.
	fn draw_dialog(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(dialog) = self.dialog.as_ref() else {
			return;
		};
		let (title, body) = match dialog {
			Dialog::ConfirmImport { path } => (
				" Import session ",
				format!("Replace current session with {path}?\n\nY/Enter confirm · N/Esc cancel"),
			),
			Dialog::SummarizeChoice { cursor, .. } => {
				let options = Dialog::SUMMARY_CHOICES
					.iter()
					.enumerate()
					.map(|(index, option)| {
						let marker = if index == *cursor { "❯ " } else { "  " };
						format!("{marker}{option}")
					})
					.collect::<Vec<_>>()
					.join("\n");
				(
					" Summarize branch? ",
					format!("{options}\n\nEnter select · Esc back to tree"),
				)
			}
			Dialog::CustomPrompt { value, .. } => (
				" Custom summarization instructions ",
				format!("{value}▌\n\nEnter submit · Esc back"),
			),
		};
		let width = chat_area.width.clamp(24, 72);
		let height = (body.lines().count() as u16 + 2).min(chat_area.height);
		let area = Rect {
			x: chat_area.x + (chat_area.width - width) / 2,
			y: chat_area.y + (chat_area.height - height) * 2 / 5,
			width,
			height,
		};
		frame.render_widget(Clear, area);
		frame.render_widget(
			Paragraph::new(body)
				.block(
					Block::bordered()
						.border_style(Style::new().fg(self.theme.accent))
						.title(Span::styled(
							title,
							Style::new()
								.fg(self.theme.accent)
								.add_modifier(Modifier::BOLD),
						)),
				)
				.wrap(Wrap { trim: false }),
			area,
		);
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

	#[tokio::test]
	async fn restored_messages_and_tool_results_are_visible() {
		let mut assistant =
			pi_ai::AssistantMessage::error(&pi_agent::types::default_model(), StopReason::Stop, "");
		assistant.error_message = None;
		assistant.content = vec![
			pi_ai::AssistantContent::Text(pi_ai::TextContent {
				text: "previous response".into(),
				text_signature: None,
			}),
			pi_ai::AssistantContent::ToolCall(pi_ai::ToolCall {
				id: "call-1".into(),
				name: "read".into(),
				arguments: serde_json::json!({"path":"file.txt"}),
				thought_signature: None,
				namespace: None,
				arguments_error: None,
			}),
		];
		let mut tool_result =
			pi_ai::ToolResultMessage::text_result("call-1", "read", "file contents", false);
		tool_result.duration_ms = Some(123);
		let agent = pi_agent::Agent::new(pi_agent::AgentOptions {
			initial_messages: vec![
				Message::user("previous question"),
				Message::Assistant(assistant),
				Message::ToolResult(tool_result),
			],
			..Default::default()
		});
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		let mode = InteractiveMode::new(handle, Vec::new(), registry, ".".into(), ui);
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::User(text) if text == "previous question"))
		);
		assert!(mode.blocks.iter().any(|block| matches!(block, ChatBlock::Assistant { text, streaming: false, .. } if text == "previous response")));
		assert!(mode.blocks.iter().any(|block| matches!(block, ChatBlock::Tool { id, output, running: false, duration_ms: Some(123), .. } if id == "call-1" && output == "file contents")));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn resume_command_switches_and_renders_saved_session() {
		let root = std::env::temp_dir().join(format!("pi-tui-resume-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let mut saved =
			crate::core::SessionManager::create_with_parent_in(&root, &root, None).unwrap();
		saved
			.append_message(&Message::user("saved question"))
			.unwrap();
		let path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let agent = pi_agent::Agent::new(pi_agent::AgentOptions::default());
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		let mut mode = InteractiveMode::new(handle, Vec::new(), registry, ".".into(), ui);
		mode.handle_slash(&format!("resume {}", path.display()));
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let event = mode.session.events.recv().await.unwrap();
				let replaced = matches!(event, SessionEvent::SessionReplaced);
				mode.handle_session_event(event);
				if replaced {
					break;
				}
			}
		})
		.await
		.unwrap();
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::User(text) if text == "saved question"))
		);
		assert_eq!(mode.cwd, root.to_string_lossy());
		mode.overlay = Some(ModelOverlay::sessions(vec![path.clone()], || false));
		mode.handle_overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let event = mode.session.events.recv().await.unwrap();
				let replaced = matches!(event, SessionEvent::SessionReplaced);
				mode.handle_session_event(event);
				if replaced {
					break;
				}
			}
		})
		.await
		.unwrap();
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::User(text) if text == "saved question"))
		);
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		drop(mode);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn session_picker_displays_and_searches_names_projects_and_first_messages() {
		let root = std::env::temp_dir().join(format!("pi-picker-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let mut named = crate::core::SessionManager::create_with_parent_in(
			&root,
			&root.join("project-alpha"),
			None,
		)
		.unwrap();
		named
			.append_message(&Message::user("first question\nsecond line"))
			.unwrap();
		named
			.append_message(&Message::user(format!(
				"{} hidden-after-preview",
				"x".repeat(300)
			)))
			.unwrap();
		let model = Model::new("test", "test", "mock", "mock", "");
		let mut assistant = pi_ai::AssistantMessage::error(&model, pi_ai::StopReason::Stop, "");
		assistant.content = vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
			text: "response-unique-marker".into(),
			text_signature: None,
		})];
		named
			.append_message(&Message::Assistant(assistant))
			.unwrap();
		named.append_session_info("Refactor auth").unwrap();
		let named_path = named.session_dir().unwrap().to_path_buf();
		let mut unnamed = crate::core::SessionManager::create_with_parent_in(
			&root,
			&root.join("project-beta"),
			None,
		)
		.unwrap();
		unnamed
			.append_message(&Message::user("inspect logs\nplease"))
			.unwrap();
		let unnamed_path = unnamed.session_dir().unwrap().to_path_buf();
		let calls = std::cell::Cell::new(0);
		let cancelled =
			ModelOverlay::sessions(vec![named_path.clone(), unnamed_path.clone()], || {
				let count = calls.get();
				calls.set(count + 1);
				count > 0
			});
		assert_eq!(cancelled.list.len(), 1);
		let mut picker = ModelOverlay::sessions(vec![named_path, unnamed_path], || false);
		let items = picker.list.filtered();
		assert_eq!(items[0].label, "Refactor auth");
		assert_eq!(items[1].label, "inspect logs please");
		assert!(
			items[0]
				.description
				.as_deref()
				.unwrap()
				.contains("first question second line")
		);
		picker.list.set_filter("project-beta");
		assert!(
			picker
				.list
				.filtered()
				.iter()
				.any(|item| item.label == "inspect logs please")
		);
		picker.list.set_filter("first question");
		assert!(
			picker
				.list
				.filtered()
				.iter()
				.any(|item| item.label == "Refactor auth")
		);
		picker.list.set_filter("hidden-after-preview");
		assert!(
			picker
				.list
				.filtered()
				.iter()
				.any(|item| item.label == "Refactor auth")
		);
		picker.list.set_filter("response-unique-marker");
		assert!(
			picker
				.list
				.filtered()
				.iter()
				.any(|item| item.label == "Refactor auth")
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[tokio::test]
	async fn resume_asks_before_trusting_project_mcp_and_escape_cancels() {
		let root = std::env::temp_dir().join(format!("pi-tui-trust-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(root.join(".pi")).unwrap();
		std::fs::write(root.join(".pi/mcp.json"), "{}").unwrap();
		let saved = crate::core::SessionManager::create_with_parent_in(&root, &root, None).unwrap();
		let path = saved.session_dir().unwrap().to_path_buf();
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			registry.clone(),
		);
		handle.configure_mcp(crate::core::tools::McpRuntime {
			assembly: Default::default(),
			allow: None,
			exclude: None,
		});
		let ui = UiConfig {
			theme: Theme::light(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		let mut mode = InteractiveMode::new(handle, Vec::new(), registry, ".".into(), ui);
		mode.resume_session(&path.to_string_lossy());
		assert!(mode.trust_prompt.is_some());
		mode.handle_terminal_event(TermEvent::Key(KeyEvent::new(
			KeyCode::Char('y'),
			KeyModifiers::CONTROL,
		)));
		assert!(mode.trust_prompt.is_some());
		mode.handle_terminal_event(TermEvent::Key(KeyEvent::new(
			KeyCode::Esc,
			KeyModifiers::empty(),
		)));
		assert!(mode.trust_prompt.is_none());
		assert!(
			mode.session
				.log
				.lock()
				.unwrap()
				.session
				.session_dir()
				.is_none()
		);
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(root).unwrap();
	}

	#[tokio::test]
	async fn switching_session_keeps_typed_input_until_ready() {
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		let mut mode = InteractiveMode::new(handle, Vec::new(), registry, ".".into(), ui);
		let (_sender, receiver) = tokio::sync::oneshot::channel();
		mode.pending_replacement = Some(receiver);
		mode.editor.set_text("/tools");
		mode.handle_terminal_event(TermEvent::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		)));
		assert_eq!(mode.editor.text(), "/tools");
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.contains("switch in progress"))
		));
		mode.pending_replacement = None;
		mode.handle_terminal_event(TermEvent::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		)));
		assert!(mode.editor.text().is_empty());
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.contains("active tools:"))
		));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn loading_sessions_allows_input_and_can_be_cancelled() {
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		let mut mode = InteractiveMode::new(handle, Vec::new(), registry, ".".into(), ui);
		let (sender, receiver) = tokio::sync::oneshot::channel();
		mode.loading_sessions = Some(receiver);
		mode.editor.set_text("hello");
		mode.handle_terminal_event(TermEvent::Key(KeyEvent::new(
			KeyCode::Enter,
			KeyModifiers::empty(),
		)));
		assert_eq!(mode.editor.text(), "hello");
		mode.handle_terminal_event(TermEvent::Key(KeyEvent::new(
			KeyCode::Esc,
			KeyModifiers::empty(),
		)));
		assert!(mode.loading_sessions.is_none());
		assert!(sender.is_closed());
		assert_eq!(mode.editor.text(), "hello");
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn new_session_replaces_live_agent_and_name_command_updates_session() {
		let agent = pi_agent::Agent::new(pi_agent::AgentOptions {
			system_prompt: Some("INITIAL_PROMPT".into()),
			..Default::default()
		});
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		let mut mode = InteractiveMode::new(
			handle,
			registry.lock().unwrap().models().to_vec(),
			registry.clone(),
			".".into(),
			ui,
		);
		mode.push_notice("old chat".into());
		mode.handle_slash("new");
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let event = mode.session.events.recv().await.unwrap();
				let replaced = matches!(event, SessionEvent::SessionReplaced);
				mode.handle_session_event(event);
				if replaced {
					break;
				}
			}
		})
		.await
		.unwrap();
		assert!(mode.blocks.is_empty());
		assert!(
			!mode
				.session
				.agent
				.snapshot()
				.system_prompt
				.contains("INITIAL_PROMPT")
		);
		mode.handle_slash("name test session");
		let event = tokio::time::timeout(Duration::from_secs(5), mode.session.wire_events.recv())
			.await
			.unwrap()
			.unwrap();
		assert!(
			matches!(event, WireEvent::SessionInfoChanged { name: Some(name) } if name == "test session")
		);
		assert_eq!(
			mode.session.shared.session_name.lock().unwrap().as_deref(),
			Some("test session")
		);
		mode.handle_slash("session");
		assert!(mode.blocks.iter().any(|block| matches!(block, ChatBlock::Notice(text) if text.contains("test session") && text.contains("(not saved)"))));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

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

	fn test_mode(handle: SessionHandle) -> InteractiveMode {
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
		};
		InteractiveMode::new(handle, Vec::new(), registry, ".".into(), ui)
	}

	/// Await the pending replacement response and run the completion path
	/// (the run-loop arm's body) so statuses land without a live event loop.
	async fn complete_pending(mode: &mut InteractiveMode) {
		let receiver = mode
			.pending_replacement
			.take()
			.expect("a replacement is pending");
		let outcome = tokio::time::timeout(Duration::from_secs(5), receiver)
			.await
			.expect("the replacement resolves")
			.unwrap_or_else(|_| Err("session task stopped".into()));
		mode.replacement_event_seen = false;
		mode.complete_replacement(outcome);
		mode.pending_replacement = None;
		mode.replacement_event_seen = false;
		mode.replacement_status = None;
		mode.replacement_kind = ReplacementKind::Switch;
	}

	/// Wait for the next `SessionReplaced` in the mode's event stream.
	async fn wait_replaced(mode: &mut InteractiveMode) {
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let event = mode.session.events.recv().await.unwrap();
				let replaced = matches!(event, SessionEvent::SessionReplaced);
				mode.handle_session_event(event);
				if replaced {
					return;
				}
			}
		})
		.await
		.expect("the replacement should land");
	}

	#[tokio::test]
	async fn clone_needs_a_leaf_and_forks_the_saved_session() {
		// /clone on a fresh session refuses with the upstream status.
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		mode.handle_slash("clone");
		// The fresh session's model entries give it a leaf, so the clone
		// reaches the unsaved-session refusal (upstream error text).
		complete_pending(&mut mode).await;
		assert!(
			mode.blocks.iter().any(
				|block| matches!(block, ChatBlock::Notice(text) if text.contains("This session has not been saved yet"))
			),
			"{:?}",
			mode.blocks
		);
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();

		// A saved session with one turn clones at the leaf: the new session
		// file exists with the parent lineage and the same transcript.
		let root = std::env::temp_dir().join(format!("pi-clone-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let mut saved =
			crate::core::SessionManager::create_with_parent_in(&root, &root, None).unwrap();
		saved
			.append_message(&Message::user("clonable question"))
			.unwrap();
		let saved_path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		mode.handle_slash(&format!("resume {}", saved_path.display()));
		wait_replaced(&mut mode).await;
		mode.blocks.clear();
		mode.handle_slash("clone");
		complete_pending(&mut mode).await;
		// The clone notice carries the upstream status.
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == "Cloned to new session")
		));
		// The live session file moved to a branched copy whose header names
		// the source, and the transcript replays.
		let new_file = mode
			.session
			.shared
			.current_session_file
			.lock()
			.unwrap()
			.clone()
			.expect("the clone is persisted");
		let (header, entries) = crate::core::session_manager::load_session_file(&new_file);
		assert_eq!(
			header.and_then(|header| header.parent_session),
			Some(saved_path.join("session.jsonl").display().to_string())
		);
		assert!(
			entries
				.iter()
				.any(|entry| matches!(&entry.kind, EntryKind::Message { message } if matches!(message.as_ref(), Message::User(user) if user.content.as_text() == "clonable question")))
		);
		assert_ne!(new_file, saved_path.join("session.jsonl"));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(root).unwrap();
	}

	#[tokio::test]
	async fn fork_selector_branches_from_the_first_user_message() {
		let root = std::env::temp_dir().join(format!("pi-fork-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let mut saved =
			crate::core::SessionManager::create_with_parent_in(&root, &root, None).unwrap();
		saved
			.append_message(&Message::user("forkable question"))
			.unwrap();
		let saved_path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		mode.handle_slash(&format!("resume {}", saved_path.display()));
		wait_replaced(&mut mode).await;
		// The selector opens with the user message and a metadata line.
		mode.handle_slash("fork");
		let Some(overlay) = mode.overlay.as_ref() else {
			panic!("the fork selector opens");
		};
		assert_eq!(overlay.kind, OverlayKind::Fork);
		let items = overlay.list.filtered();
		assert_eq!(items.len(), 1);
		assert_eq!(items[0].label, "forkable question");
		assert_eq!(items[0].description.as_deref(), Some("Message 1 of 1"));
		// Confirming forks at the message's parent (null → fresh session with
		// the parent lineage) and pre-fills the editor with the message text.
		mode.handle_overlay_key(key_event("enter"));
		complete_pending(&mut mode).await;
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == "Forked to new session")
		));
		assert_eq!(mode.editor.text(), "forkable question");
		let new_file = mode
			.session
			.shared
			.current_session_file
			.lock()
			.unwrap()
			.clone()
			.expect("the fork target persists");
		let (header, entries) = crate::core::session_manager::load_session_file(&new_file);
		assert_eq!(
			header.and_then(|header| header.parent_session),
			Some(saved_path.join("session.jsonl").display().to_string())
		);
		// Fork before the first message: a fresh session, no replayed entries.
		assert!(
			!entries
				.iter()
				.any(|entry| matches!(&entry.kind, EntryKind::Message { .. })),
			"the fork-before-first-message session starts empty"
		);
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(root).unwrap();
	}

	#[tokio::test]
	async fn fork_without_messages_reports_status() {
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		mode.handle_slash("fork");
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == "No messages to fork from")
		));
		assert!(mode.overlay.is_none());
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn import_confirms_and_replaces_with_the_copied_session() {
		let root = std::env::temp_dir().join(format!("pi-import-tui-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let header = serde_json::json!({"type":"session","version":3,"id":"imported","timestamp":"2026-01-01T00:00:00Z","cwd":root.display().to_string()});
		let first = serde_json::json!({"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"imported question","timestamp":1}});
		let source = root.join("incoming.jsonl");
		std::fs::write(&source, format!("{header}\n{first}\n")).unwrap();
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		// No argument: the upstream usage line.
		mode.handle_slash("import");
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == "Usage: /import <path.jsonl>"))
		);
		// Missing file: the confirm dialog still opens, then the error lands
		// with the upstream text after confirmation.
		mode.handle_slash(&format!("import {}", root.join("missing.jsonl").display()));
		let Some(Dialog::ConfirmImport { path }) = mode.dialog.clone() else {
			panic!("the import confirm opens");
		};
		assert!(path.ends_with("missing.jsonl"));
		mode.handle_dialog_key(key_event("y"));
		complete_pending(&mut mode).await;
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::Notice(text) if text.contains("Failed to import session: File not found"))),
			"{:?}",
			mode.blocks
		);
		// A real file: confirming replaces the runtime and reports the status.
		mode.blocks.clear();
		mode.handle_slash(&format!("import {}", source.display()));
		mode.handle_dialog_key(key_event("y"));
		complete_pending(&mut mode).await;
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == format!("Session imported from: {}", source.display()))),
			"{:?}",
			mode.blocks
		);
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::User(text) if text == "imported question"))
		);
		// The import was copied into the sessions storage.
		let live = mode
			.session
			.shared
			.current_session_file
			.lock()
			.unwrap()
			.clone()
			.unwrap();
		let imported_text = std::fs::read_to_string(&live).unwrap();
		assert!(imported_text.contains("imported question"));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(root).unwrap();
		let _ = crate::config::sessions_dir();
	}

	#[tokio::test]
	async fn import_dialog_escape_cancels() {
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		mode.handle_slash("import /tmp/whatever.jsonl");
		assert!(mode.dialog.is_some());
		mode.handle_dialog_key(key_event("esc"));
		assert!(mode.dialog.is_none());
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == "Import cancelled")
		));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn export_writes_the_branch_jsonl_and_reports_the_path() {
		let root = std::env::temp_dir().join(format!("pi-export-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		{
			let mut guard = mode.session.log.lock().unwrap();
			guard
				.session
				.append_message(&Message::user("exported question"))
				.unwrap();
			guard
				.session
				.append_message(&Message::user("second entry"))
				.unwrap();
		}
		let target = root.join("branch.jsonl");
		mode.handle_slash(&format!("export {}", target.display()));
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == format!("Session exported to: {}", target.display()))),
			"{:?}",
			mode.blocks
		);
		let lines: Vec<serde_json::Value> = std::fs::read_to_string(&target)
			.unwrap()
			.lines()
			.map(|line| serde_json::from_str(line).unwrap())
			.collect();
		// Header + the branch with re-chained parent ids (a fresh session
		// carries model/thinking bookkeeping entries ahead of the messages;
		// the first entry roots at null like upstream's serializeSessionBranch).
		assert_eq!(lines[0]["type"], "session");
		assert_eq!(lines[1]["parentId"], serde_json::Value::Null);
		for pair in lines[1..].windows(2) {
			assert_eq!(pair[1]["parentId"], pair[0]["id"]);
		}
		assert!(
			lines.iter().any(|entry| {
				entry["message"]["content"]
					.as_array()
					.is_some_and(|blocks| {
						blocks.iter().any(|block| block["text"] == "second entry")
					})
			}),
			"the full branch exports: {:?}",
			lines
		);
		// Quoted paths keep their spaces (getPathCommandArgument).
		let quoted = root.join("with space.jsonl");
		mode.blocks.clear();
		mode.handle_slash(&format!("export \"{}\"", quoted.display()));
		assert!(quoted.is_file(), "the quoted path was used verbatim");
		// Failures carry the upstream prefix (a parent directory that is a
		// file cannot be created, unlike a missing one — upstream mkdirs
		// recursively too).
		mode.blocks.clear();
		mode.handle_slash(&format!("export {}/out.jsonl", target.display()));
		assert!(
			mode.blocks
				.iter()
				.any(|block| matches!(block, ChatBlock::Notice(text) if text.starts_with("Failed to export session:"))),
			"{:?}",
			mode.blocks
		);
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(root).unwrap();
	}

	#[tokio::test]
	async fn tree_selector_opens_and_reports_empty_sessions() {
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		// A fresh session carries the model/thinking bookkeeping entries, so
		// the selector opens (upstream `tree.length === 0` is about raw
		// entries); the default filter leaves no visible rows.
		mode.handle_slash("tree");
		let tree = mode
			.tree
			.as_ref()
			.expect("the selector opens on a fresh session");
		assert_eq!(tree.len(), 0);
		assert!(
			tree.render_lines(80, &mode.theme, 10)[0]
				.spans
				.iter()
				.any(|span| span.content.contains("No entries found"))
		);
		mode.handle_tree_key(key_event("esc"));
		assert!(mode.tree.is_none());
		// With a user message the selector shows it pre-selected at the leaf;
		// selecting the current leaf is a no-op with the upstream status.
		{
			let mut guard = mode.session.log.lock().unwrap();
			guard
				.session
				.append_message(&Message::user("only entry"))
				.unwrap();
		}
		mode.handle_slash("tree");
		let leaf = mode
			.session
			.log
			.lock()
			.unwrap()
			.session
			.leaf_id()
			.map(str::to_owned)
			.unwrap();
		assert_eq!(
			mode.tree.as_ref().unwrap().selected_id(),
			Some(leaf.as_str())
		);
		mode.handle_tree_key(key_event("enter"));
		assert!(mode.tree.is_none());
		assert!(mode.blocks.iter().any(
			|block| matches!(block, ChatBlock::Notice(text) if text.as_str() == "Already at this point")
		));
		mode.handle_slash("tree");
		assert!(mode.tree.is_some());
		mode.handle_tree_key(key_event("esc"));
		assert!(mode.tree.is_none());
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn tree_summarize_prompt_escapes_back_to_the_tree() {
		let root = std::env::temp_dir().join(format!("pi-tree-tui-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&root).unwrap();
		let mut saved =
			crate::core::SessionManager::create_with_parent_in(&root, &root, None).unwrap();
		saved
			.append_message(&Message::user("first question"))
			.unwrap();
		saved
			.append_message(&Message::user("second question"))
			.unwrap();
		let saved_path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::SessionManager::in_memory(),
			Arc::new(Mutex::new(ModelRegistry::load(None))),
		);
		let mut mode = test_mode(handle);
		mode.handle_slash(&format!("resume {}", saved_path.display()));
		wait_replaced(&mut mode).await;
		let first = mode
			.session
			.log
			.lock()
			.unwrap()
			.session
			.entries()
			.iter()
			.filter(|entry| matches!(&entry.kind, EntryKind::Message { message } if matches!(message.as_ref(), Message::User(user) if user.content.as_text() == "first question")))
			.find_map(|entry| entry.id().map(str::to_owned))
			.unwrap();
		// Pick a non-leaf entry: the three-way prompt opens.
		mode.show_tree_selector(Some(&first));
		mode.handle_tree_key(key_event("enter"));
		let Some(Dialog::SummarizeChoice { target_id, .. }) = mode.dialog.clone() else {
			panic!("the summarize prompt opens");
		};
		assert_eq!(target_id, first);
		// Esc returns to the tree selector with the original selection.
		mode.handle_dialog_key(key_event("esc"));
		assert!(mode.dialog.is_none());
		assert_eq!(
			mode.tree.as_ref().unwrap().selected_id(),
			Some(first.as_str())
		);
		// "Summarize with custom prompt" opens the single-line editor; Esc
		// loops back to the choice.
		mode.handle_tree_key(key_event("enter"));
		mode.handle_dialog_key(key_event("down"));
		mode.handle_dialog_key(key_event("down"));
		mode.handle_dialog_key(key_event("enter"));
		assert!(matches!(mode.dialog, Some(Dialog::CustomPrompt { .. })));
		mode.handle_dialog_key(key_event("esc"));
		assert!(matches!(mode.dialog, Some(Dialog::SummarizeChoice { .. })));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(root).unwrap();
	}
}
