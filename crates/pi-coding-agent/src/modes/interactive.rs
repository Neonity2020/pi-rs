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

use std::collections::VecDeque;
use std::path::PathBuf;
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
use pi_ai::{Message, Model, StopReason, ThinkingLevel};
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
use crate::core::session_manager::{EntryKind, SessionInfo};
use crate::core::slash_commands::BUILTIN_SLASH_COMMANDS;
use crate::core::ui_config::UiConfig;
use crate::modes::session_selector::{
	SelectorAction, SessionScope, SessionSelectorOverlay, StatusKind,
};

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
	/// A bordered markdown document (`DynamicBorder` + bold accent title +
	/// `Markdown` upstream): `/changelog`'s "What's New" and `/hotkeys`'s
	/// "Keyboard Shortcuts".
	Doc {
		title: String,
		text: String,
		rendered: Option<(usize, Vec<Line<'static>>)>,
	},
	/// Passthrough bash (`!cmd`): live output tail while running, full text
	/// (or the spill-file note) once done.
	Bash {
		command: String,
		output: String,
		status: String,
		rendered: Option<(usize, Vec<Line<'static>>)>,
	},
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
	/// `/settings` single-line value editor (upstream settings-submenu
	/// input); Esc returns to the settings selector, Enter applies.
	SettingInput { key: String, value: String },
}

impl Dialog {
	const SUMMARY_CHOICES: [&str; 3] = ["No summary", "Summarize", "Summarize with custom prompt"];
}

/// Extension dialog (round 42): `ui.select`/`ui.confirm`/`ui.input` from an
/// extension host, answered through the bridge's dialog registry (upstream
/// `showExtensionSelector`/`showExtensionConfirm`/`showExtensionEditor`).
/// Modal over every other overlay; further requests queue until it closes.
struct ExtensionDialogState {
	request: crate::core::extensions::ExtensionDialogRequest,
	/// Select cursor.
	cursor: usize,
	/// Input editor value.
	value: String,
}

impl ExtensionDialogState {
	fn new(request: crate::core::extensions::ExtensionDialogRequest) -> Self {
		Self {
			request,
			cursor: 0,
			value: String::new(),
		}
	}
}

/// `/settings` — one entry of the settings selector (upstream
/// `SettingsSelectorComponent`'s value list, cut to the settings pi-rs
/// actually persists).
struct SettingEntry {
	key: &'static str,
	editor: SettingEditor,
	/// Hot-applies to the running session (upstream's live callbacks:
	/// thinking level, theme, tree filter). Everything else honestly reports
	/// that a restart picks it up.
	instant: bool,
}

/// How Enter edits the entry (upstream cycles enums inline and opens a
/// single-line editor for strings/numbers).
#[derive(Clone)]
enum SettingEditor {
	/// Enter cycles through the values (enums, booleans).
	Cycle(Vec<String>),
	/// Enter opens the single-line editor (strings, numbers).
	Input,
}

/// `/settings` selector state: the list plus its fixed entry metadata.
struct SettingsOverlay {
	list: SelectList,
	entries: Vec<SettingEntry>,
}

/// `/scoped-models` multi-select (upstream `ScopedModelsSelectorComponent`,
/// reduced to the session scope — pi-rs persists no `enabledModels`
/// setting, so the selection applies until the next session replacement).
struct ScopedModelsOverlay {
	search: String,
	/// The full catalog (upstream refreshes model catalogs in place; pi-rs
	/// snapshots at open).
	all: Vec<Model>,
	default_key: String,
	list: SelectList,
	checked: std::collections::BTreeSet<String>,
}

impl ScopedModelsOverlay {
	fn new(all: &[Model], checked: std::collections::BTreeSet<String>, default_key: &str) -> Self {
		let mut list = SelectList::new(scoped_model_items(all, &checked, default_key), 12);
		list.set_no_match_message("No matching models");
		list.set_mark_value(Some(default_key.to_string()));
		Self {
			search: String::new(),
			all: all.to_vec(),
			default_key: default_key.to_string(),
			list,
			checked,
		}
	}

	/// Rebuild the rows after a toggle; the filter and cursor survive
	/// (`set_items` keeps the selection index and refilters).
	fn refresh_items(&mut self) {
		let items = scoped_model_items(&self.all, &self.checked, &self.default_key);
		self.list.set_items(items);
	}
}

/// `/trust` selector context (upstream `TrustSelectorComponent`): the header
/// lines are precomputed — cwd, saved decision, effective session state.
struct TrustOverlayInfo {
	cwd: std::path::PathBuf,
	header: Vec<String>,
}

/// Which session-replacement command is in flight (`begin_replacement`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplacementKind {
	New,
	Switch,
	Fork,
	Clone,
	Import,
	/// `/reload`: the same session path re-opened through the replacement
	/// machinery (the closest port of upstream's runtime rebuild).
	Reload,
}

/// The response channel of a running `/tree` navigation plus the entry to
/// re-select when the branch summary is cancelled.
type PendingTreeNav = (
	tokio::sync::oneshot::Receiver<Result<TreeNavResult, String>>,
	Option<String>,
);

/// Slash-command autocomplete — the editor-typed `/` menu (upstream
/// `tui/src/autocomplete.ts` `CombinedAutocompleteProvider` slash branch plus
/// the interactive-mode fuzzy items): while the first token is still
/// `/name`, the menu lists commands fuzzy-filtered by the text after the
/// slash; confirming completes to `/name ` in the editor.
struct SlashMenu {
	list: SelectList,
}

impl SlashMenu {
	fn new(items: Vec<SelectItem>) -> Self {
		let mut list = SelectList::new(items, 8);
		list.set_no_match_message("No matching commands");
		Self { list }
	}
}

/// The slash menu listing: the static builtin table plus the dynamic
/// resource commands (prompt templates as `/<name>`, skills as
/// `/skill:<name>` gated on the `enableSkillCommands` setting) and the
/// registered extension commands — upstream
/// `createBaseAutocompleteProvider` (interactive-mode.ts:788-821).
fn slash_menu_items(
	resources: &crate::core::agent_session::SessionResources,
	enable_skill_commands: bool,
	extension_commands: &[(String, String)],
) -> Vec<SelectItem> {
	let mut items: Vec<SelectItem> = BUILTIN_SLASH_COMMANDS
		.iter()
		.map(|info| SelectItem {
			value: info.name.to_string(),
			label: format!("/{}", info.name),
			description: Some(info.description.to_string()),
		})
		.collect();
	for command in crate::core::slash_commands::dynamic_slash_commands(
		&resources.prompt_templates,
		&resources.skills,
		enable_skill_commands,
		extension_commands,
	) {
		let mut description = command.description;
		if let Some(hint) = &command.argument_hint {
			description.push_str(&format!(" ({hint})"));
		}
		items.push(SelectItem {
			value: command.name.clone(),
			label: format!("/{}", command.name),
			description: Some(description),
		});
	}
	items
}

/// The live fuzzy query when the editor text qualifies for the slash menu:
/// starts with `/`, still a single token (no whitespace yet — argument
/// completion has no provider in this port) and not a lone `/` line with
/// other lines. `None` closes the menu.
fn slash_command_query(text: &str) -> Option<&str> {
	let command = text.strip_prefix('/')?;
	if command.contains(char::is_whitespace) {
		return None;
	}
	Some(command)
}

/// Upstream `applyCompletion` slash branch: the selected name completes the
/// token and a trailing space cues the argument position. The qualifying
/// editor text IS the token, so the replacement is total.
fn apply_slash_completion(name: &str) -> String {
	format!("/{name} ")
}

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
	// Slash-command autocomplete anchored above the editor (live-filtered).
	slash_menu: Option<SlashMenu>,
	// Session selector overlay (/resume): owns its own state; async session
	// listing and file mutations flow through the typed channel below.
	session_overlay: Option<SessionSelectorOverlay>,
	// /trust selector: the decision list plus the precomputed header lines.
	trust_overlay: Option<(SelectList, TrustOverlayInfo)>,
	// /settings selector.
	settings_overlay: Option<SettingsOverlay>,
	// /scoped-models multi-select.
	scoped_overlay: Option<ScopedModelsOverlay>,
	// Extension dialog (ui.select/confirm/input) plus its request queue —
	// modal over every other overlay; answers go through the bridge registry.
	extension_dialog: Option<ExtensionDialogState>,
	pending_extension_dialogs: VecDeque<crate::core::extensions::ExtensionDialogRequest>,
	// ui.setStatus slot (round 43): the host shim drops the upstream `key`,
	// so this holds the latest text; `None` hides the footer status line.
	extension_status: Option<String>,
	session_list_tx: mpsc::UnboundedSender<SessionListMsg>,
	session_list_rx: mpsc::UnboundedReceiver<SessionListMsg>,
	/// `@file` image attachments riding on the next prompt (`@file` text
	/// files are inlined into the text; images wait for the first submit).
	pending_images: Vec<pi_ai::ImageContent>,
	/// A passthrough bash command is in flight (upstream `isBashRunning`).
	bash_running: bool,
	// External editor (ctrl+g): the terminal is suspended while the editor
	// runs; the outcome comes back through the channel.
	external_editor_cmd: String,
	suspended: bool,
	pending_external_editor: bool,
	editor_tx: mpsc::UnboundedSender<ExternalEditorOutcome>,
	editor_rx: mpsc::UnboundedReceiver<ExternalEditorOutcome>,
	// UI-only messages from background tasks (git branch watcher).
	ui_rx: mpsc::UnboundedReceiver<String>,
	ui_tx: mpsc::UnboundedSender<String>,
}

/// Session listing progress/results for the selector overlay (upstream loads
/// inline with an AbortController; the port spawns a blocking task and
/// streams through this channel).
enum SessionListMsg {
	Progress {
		scope: SessionScope,
		loaded: usize,
		total: usize,
		partial: Vec<SessionInfo>,
	},
	Loaded {
		scope: SessionScope,
		sessions: Vec<SessionInfo>,
	},
	Status {
		kind: StatusKind,
		text: String,
	},
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

/// The thinking levels the current model accepts (upstream
/// `getAvailableThinkingLevels`): everything for reasoning models, off only
/// otherwise.
fn thinking_levels_for_model(model: &Model) -> Vec<ThinkingLevel> {
	if model.reasoning {
		vec![
			ThinkingLevel::Off,
			ThinkingLevel::Minimal,
			ThinkingLevel::Low,
			ThinkingLevel::Medium,
			ThinkingLevel::High,
			ThinkingLevel::Xhigh,
			ThinkingLevel::Max,
		]
	} else {
		vec![ThinkingLevel::Off]
	}
}

/// Passthrough bash block (`!cmd`): a `bash:` header with the status, then
/// the output preview — live tail while running (upstream
/// `bash-execution.ts` PREVIEW_LINES), first lines once done.
fn render_bash_block(
	command: &str,
	output: &str,
	status: &str,
	width: usize,
	theme: &Theme,
) -> Vec<Line<'static>> {
	let _ = width;
	let mut lines = Vec::new();
	let running = status == "running";
	let status_color = match status {
		"running" => theme.accent,
		"done" => theme.success,
		_ => theme.error,
	};
	lines.push(Line::from(vec![
		Span::styled("  bash: ", Style::new().fg(theme.tool)),
		Span::styled(command.to_string(), Style::new().fg(theme.assistant)),
		Span::styled(format!("  ({status})"), Style::new().fg(status_color)),
	]));
	if !output.is_empty() {
		let out_lines: Vec<&str> = output.lines().collect();
		let preview = if running {
			TOOL_RUNNING_PREVIEW_LINES
		} else {
			TOOL_PREVIEW_LINES
		};
		let (start, shown) = if running && out_lines.len() > preview {
			(out_lines.len() - preview, preview)
		} else {
			(0, out_lines.len().min(preview))
		};
		for line in out_lines.iter().skip(start).take(shown) {
			lines.push(Line::from(Span::styled(
				format!("    {line}"),
				Style::new().fg(theme.tool_output),
			)));
		}
		let remaining = out_lines.len() - shown;
		if remaining > 0 {
			lines.push(Line::from(Span::styled(
				format!("  … ({remaining} more lines)"),
				Style::new().fg(theme.muted),
			)));
		}
	}
	lines
}

/// Chat blocks for a restored/replaced session's messages: tool calls pair
/// with their results by id (calls without a persisted result render with
/// empty output, results without a call are skipped).
fn blocks_from_messages(messages: &[Message]) -> Vec<ChatBlock> {
	let results: std::collections::HashMap<&str, &pi_ai::ToolResultMessage> = messages
		.iter()
		.filter_map(|m| match m {
			Message::ToolResult(result) => Some((result.tool_call_id.as_str(), result)),
			_ => None,
		})
		.collect();
	let mut blocks = Vec::new();
	for message in messages {
		match message {
			Message::System(_) | Message::ToolResult(_) => {}
			Message::User(user) => blocks.push(ChatBlock::User(user.content.as_text())),
			Message::Assistant(assistant) => {
				blocks.push(ChatBlock::Assistant {
					text: assistant.text(),
					thinking: assistant.thinking(),
					streaming: false,
					stop_note: None,
					error: assistant.error_message.clone(),
					rendered: None,
				});
				for call in assistant.tool_calls() {
					let result = results.get(call.id.as_str());
					blocks.push(ChatBlock::Tool {
						id: call.id.clone(),
						name: call.name.clone(),
						args: call.arguments.clone(),
						running: false,
						is_error: result.is_some_and(|r| r.is_error),
						output: result.map(|r| r.text()).unwrap_or_default(),
						started: None,
						duration_ms: result.and_then(|r| r.duration_ms).map(u128::from),
						rendered: None,
					});
				}
			}
		}
	}
	blocks
}

/// Accumulated usage over a restored transcript (the TUI counters restart
/// from the messages after a session replacement).
fn usage_of_messages(messages: &[Message]) -> (u64, u64, u64, u64, f64) {
	let mut usage = (0u64, 0u64, 0u64, 0u64, 0.0f64);
	for message in messages {
		if let Message::Assistant(assistant) = message {
			usage.0 += assistant.usage.input;
			usage.1 += assistant.usage.output;
			usage.2 += assistant.usage.cache_read;
			usage.3 += assistant.usage.cache_write;
			usage.4 += assistant.usage.cost.total.unwrap_or(0.0);
		}
	}
	usage
}

/// Terminal input reader, restartable: the editor suspension pauses it by
/// closing this sender's counterpart (dropping the EventStream releases the
/// stdin read so the external editor owns keystrokes).
fn spawn_term_reader(tx: mpsc::UnboundedSender<TermEvent>) -> mpsc::UnboundedSender<()> {
	let (pause_tx, mut pause_rx) = mpsc::unbounded_channel();
	tokio::spawn(async move {
		let mut stream = crossterm::event::EventStream::new();
		use futures::StreamExt;
		loop {
			tokio::select! {
				event = stream.next() => {
					match event {
						Some(Ok(event)) => {
							if tx.send(event).is_err() {
								break;
							}
						}
						_ => break,
					}
				}
				_ = pause_rx.recv() => break,
			}
		}
	});
	pause_tx
}

/// External editor outcome (upstream `ExternalEditorResult`): edited
/// content on success; failure leaves the prompt untouched.
enum ExternalEditorOutcome {
	Complete(String),
	Failed,
}

/// Port of `editInExternalEditor` (`external-editor.ts`): write the prompt
/// into a fresh `pi-editor-*` temp dir, run the editor with inherited
/// stdio (the caller has suspended the TUI), read back on success. BOM and
/// one trailing newline are stripped; spawn errors, non-zero exits and
/// unreadable files all count as failed (upstream propagates the read
/// throw; the port degrades to "failed").
async fn run_external_editor(command: &str, content: &str) -> ExternalEditorOutcome {
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.subsec_nanos();
	let directory =
		std::env::temp_dir().join(format!("pi-editor-{}-{}", std::process::id(), nanos));
	if std::fs::create_dir_all(&directory).is_err() {
		return ExternalEditorOutcome::Failed;
	}
	let file_path = directory.join("prompt.md");
	let attempt = async {
		std::fs::write(&file_path, content).map_err(|e| e.to_string())?;
		let mut parts = command.split_whitespace();
		let program = parts
			.next()
			.ok_or_else(|| "empty editor command".to_string())?;
		let status = tokio::process::Command::new(program)
			.args(parts)
			.arg(&file_path)
			.stdin(std::process::Stdio::inherit())
			.stdout(std::process::Stdio::inherit())
			.stderr(std::process::Stdio::inherit())
			.status()
			.await
			.map_err(|e| e.to_string())?;
		if !status.success() {
			return Err("editor exited non-zero".to_string());
		}
		let text = std::fs::read_to_string(&file_path).map_err(|e| e.to_string())?;
		Ok(ExternalEditorOutcome::Complete(strip_editor_text(&text)))
	}
	.await;
	let _ = std::fs::remove_dir_all(&directory);
	attempt.unwrap_or(ExternalEditorOutcome::Failed)
}

/// `stripBom` + one trailing newline (upstream `.replace(/\n$/, "")`).
fn strip_editor_text(text: &str) -> String {
	let text = text.strip_prefix('\u{feff}').unwrap_or(text);
	text.strip_suffix('\n').unwrap_or(text).to_string()
}

/// Delete a session file, trying the `trash` CLI first, then unlink
/// (upstream `deleteSessionFile`).
fn delete_session_file(path: &std::path::Path) -> Result<&'static str, String> {
	let display = path.to_string_lossy();
	let mut command = std::process::Command::new("trash");
	if display.starts_with('-') {
		command.arg("--");
	}
	command.arg(path);
	let trash_result = command.output();
	let trash_hint = match &trash_result {
		Ok(output) => String::from_utf8_lossy(&output.stderr)
			.lines()
			.next()
			.map(str::trim)
			.filter(|line| !line.is_empty())
			.map(|line| format!("trash: {}", &line[..line.len().min(200)])),
		Err(error) => Some(format!("trash: {error}")),
	};
	// If trash reports success, or the file is gone afterwards, treat it as
	// successful.
	let gone = !path.exists();
	let trash_ok = matches!(&trash_result, Ok(output) if output.status.success());
	if trash_ok || gone {
		return Ok("Session moved to trash");
	}
	std::fs::remove_file(path).map_err(|error| match trash_hint {
		Some(hint) => format!("{error} ({hint})"),
		None => error.to_string(),
	})?;
	Ok("Session deleted")
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
	SessionResume,
	EditorExternal,
	CopyMessage,
	ModelCycleForward,
	ModelCycleBackward,
	SessionTree,
	SessionFork,
	PasteClipboard,
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
	if keybindings.matches("app.session.resume", key) {
		return Some(GlobalAction::SessionResume);
	}
	if keybindings.matches("app.editor.external", key) {
		return Some(GlobalAction::EditorExternal);
	}
	if keybindings.matches("app.message.copy", key) {
		return Some(GlobalAction::CopyMessage);
	}
	if keybindings.matches("app.model.cycleForward", key) {
		return Some(GlobalAction::ModelCycleForward);
	}
	if keybindings.matches("app.model.cycleBackward", key) {
		return Some(GlobalAction::ModelCycleBackward);
	}
	if keybindings.matches("app.session.tree", key) {
		return Some(GlobalAction::SessionTree);
	}
	if keybindings.matches("app.session.fork", key) {
		return Some(GlobalAction::SessionFork);
	}
	if keybindings.matches("app.clipboard.pasteImage", key) {
		return Some(GlobalAction::PasteClipboard);
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
		let (session_list_tx, session_list_rx) = mpsc::unbounded_channel();
		let (editor_tx, editor_rx) = mpsc::unbounded_channel();
		let external_editor_cmd = ui.external_editor.clone();
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
			slash_menu: None,
			session_overlay: None,
			trust_overlay: None,
			settings_overlay: None,
			scoped_overlay: None,
			extension_dialog: None,
			pending_extension_dialogs: VecDeque::new(),
			extension_status: None,
			session_list_tx,
			session_list_rx,
			pending_images: ui.initial_images,
			bash_running: false,
			external_editor_cmd,
			suspended: false,
			pending_external_editor: false,
			editor_tx,
			editor_rx,
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
		let mut term_pause = spawn_term_reader(term_tx.clone());

		loop {
			if !self.suspended {
				guard.terminal.draw(|frame| self.draw(frame))?;
			}

			// ctrl+g: pause the input reader first (an EventStream alive in
			// the background would race the editor for keystrokes), hand the
			// terminal over, then run the editor in the background.
			if self.pending_external_editor {
				self.pending_external_editor = false;
				let _ = term_pause.send(());
				guard.suspend();
				self.suspended = true;
				println!(
					"Launching external editor: {}\nPi will resume when the editor exits.",
					self.external_editor_cmd
				);
				self.spawn_external_editor();
			}

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
					match event {
						Ok(wire) => self.handle_wire_event(wire),
						Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
							self.push_notice(format!("event lag: dropped {count} wire events"));
						}
						Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
					}
				}
				Some(branch) = self.ui_rx.recv() => {
					self.branch = Some(branch);
				}
				Some(msg) = self.session_list_rx.recv() => {
					self.handle_session_list_msg(msg);
				}
				Some(outcome) = self.editor_rx.recv() => {
					// The editor exited: bring the terminal back before any
					// draw, then apply the edited prompt (failed edits leave
					// the editor content unchanged, like upstream).
					if self.suspended {
						guard.resume()?;
						self.suspended = false;
						term_pause = spawn_term_reader(term_tx.clone());
					}
					if let ExternalEditorOutcome::Complete(content) = outcome {
						self.editor.set_text(content);
					}
				}
				_ = tokio::time::sleep(Duration::from_millis(TICK_MS)) => {
					self.tick = self.tick.wrapping_add(1);
					self.branch_refresh_tick += 1;
					if self.branch_refresh_tick >= 375 {
						self.branch_refresh_tick = 0;
						self.refresh_branch();
					}
					if let Some(overlay) = self.session_overlay.as_mut() {
						overlay.tick();
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
		guard.restore();
		// Upstream `formatResumeCommand` prints the resume invocation on
		// exit; the port points at the session directory name, which
		// `--resume` resolves by prefix (upstream's `--session <id>` flag
		// and the `--session-dir` argument have no port counterpart — a
		// custom `PI_RS_SESSION_DIR` must be re-exported by the user).
		if let Some(session_dir) = self
			.session
			.current_session_file()
			.and_then(|file| file.parent().map(std::path::Path::to_path_buf))
			&& let Some(name) = session_dir.file_name()
		{
			println!(
				"\x1b[2mTo resume this session:\x1b[22m pi-rs --resume {}",
				name.to_string_lossy()
			);
		}
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
			// Extension dialogs are the most modal surface: a permission gate
			// blocks the agent run until it is answered.
			if self.extension_dialog.is_some() {
				self.handle_extension_dialog_key(*key);
				return;
			}
			if self.session_overlay.is_some() {
				self.handle_session_overlay_key(*key);
				return;
			}
			if self.trust_overlay.is_some() {
				self.handle_trust_overlay_key(*key);
				return;
			}
			// The dialogs (import confirm, summarize prompts, the settings
			// value editor) sit above every list overlay they may have been
			// opened from.
			if self.dialog.is_some() {
				self.handle_dialog_key(*key);
				return;
			}
			if self.settings_overlay.is_some() {
				self.handle_settings_overlay_key(*key);
				return;
			}
			if self.scoped_overlay.is_some() {
				self.handle_scoped_models_key(*key);
				return;
			}
			if self.overlay.is_some() {
				self.handle_overlay_key(*key);
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
			// Slash menu: intercept only the completion keys — every other
			// key (typing included) reaches the editor so the menu keeps
			// live-filtering, like upstream's editor-driven autocomplete.
			if self.slash_menu.is_some() && self.handle_slash_menu_key(*key) {
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
			self.update_slash_menu();
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
				// Escape always aborts/consumes (pi behavior), even idle: a
				// passthrough bash command first, then the agent run.
				if self.bash_running {
					let _ = self.session.cmd_tx.send(SessionCommand::AbortBash);
				} else if self.working {
					let _ = self.session.cmd_tx.send(SessionCommand::Abort);
				}
			}
			// pi divergence: upstream app.clear clears the editor (double
			// press exits); pi-rs keeps its simpler ctrl+c exits.
			GlobalAction::Clear | GlobalAction::Exit => self.quit = true,
			GlobalAction::ModelSelect => {
				let current = self.session.live_agent().model().key();
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
			GlobalAction::SessionResume => self.open_session_selector(),
			// Handled in run(): suspending the terminal must happen where
			// the TerminalGuard lives.
			GlobalAction::EditorExternal => self.pending_external_editor = true,
			GlobalAction::CopyMessage => self.handle_copy_command(),
			GlobalAction::ModelCycleForward => self.cycle_model(1),
			GlobalAction::ModelCycleBackward => self.cycle_model(-1),
			GlobalAction::SessionTree => self.show_tree_selector(None),
			GlobalAction::SessionFork => self.open_fork_selector(),
			GlobalAction::PasteClipboard => self.handle_clipboard_paste(),
		}
		true
	}

	fn handle_clipboard_paste(&mut self) {
		match crate::core::clipboard::paste_from_clipboard() {
			Ok(crate::core::clipboard::PasteContent::FilePaths(paths)) => {
				let is_bash = self.editor.text().starts_with('!');
				let text = if is_bash {
					paths
						.iter()
						.map(|p| quote_if_needed(p))
						.collect::<Vec<_>>()
						.join(" ")
				} else {
					paths.join("\n")
				};
				self.insert_text_with_spaces(&text);
			}
			Ok(crate::core::clipboard::PasteContent::ImageFile(path)) => {
				self.insert_text_with_spaces(&path.to_string_lossy());
			}
			Ok(crate::core::clipboard::PasteContent::Text(text)) => {
				self.editor.insert_str(&text);
			}
			Ok(crate::core::clipboard::PasteContent::Empty) => {}
			Err(err) => {
				self.push_notice(format!("Failed to paste from clipboard: {err}"));
			}
		}
	}

	fn insert_text_with_spaces(&mut self, text: &str) {
		let leading = match self.editor.char_before_cursor() {
			Some(c) if !c.is_whitespace() => " ",
			_ => "",
		};
		let trailing = match self.editor.char_after_cursor() {
			Some(c) if !c.is_whitespace() => " ",
			_ => "",
		};
		self.editor
			.insert_str(&format!("{leading}{text}{trailing}"));
	}

	// -- external editor (ctrl+g, external-editor.ts) ------------------------------

	fn spawn_external_editor(&mut self) {
		let command = self.external_editor_cmd.clone();
		let content = self.editor.text().to_string();
		let tx = self.editor_tx.clone();
		tokio::spawn(async move {
			let outcome = run_external_editor(&command, &content).await;
			let _ = tx.send(outcome);
		});
	}

	// -- session selector (/resume) ------------------------------------------------

	/// Open the selector and kick off the current-folder listing (upstream
	/// `showSessionSelector` loads scope "current" immediately).
	fn open_session_selector(&mut self) {
		if self.session_overlay.is_some() {
			return;
		}
		if self.session.session_dir.is_none() {
			self.push_notice("no saved sessions (--no-session)".into());
			return;
		}
		self.session_overlay = Some(SessionSelectorOverlay::new(
			self.session.current_session_file(),
		));
		self.load_sessions(SessionScope::Current);
	}

	/// Re-list the active scope after a delete/rename (upstream
	/// `refreshSessionsAfterMutation`).
	fn reload_sessions(&mut self) {
		if let Some(overlay) = self.session_overlay.as_ref() {
			let scope = overlay.scope;
			self.load_sessions(scope);
		}
	}

	fn load_sessions(&mut self, scope: SessionScope) {
		if let Some(overlay) = self.session_overlay.as_mut() {
			overlay.set_loading(scope, true);
		}
		let tx = self.session_list_tx.clone();
		let cwd = std::path::PathBuf::from(&self.cwd);
		tokio::task::spawn_blocking(move || {
			let root = crate::config::sessions_dir();
			let mut progress = |loaded: usize, total: usize, partial: &[SessionInfo]| {
				let _ = tx.send(SessionListMsg::Progress {
					scope,
					loaded,
					total,
					partial: partial.to_vec(),
				});
			};
			// Current folder filters by header cwd (pi-rs keeps one sessions
			// root; upstream has per-cwd directories), All lists everything.
			let sessions = crate::core::session_manager::list_sessions_progress(
				&root,
				match scope {
					SessionScope::Current => Some(&cwd),
					SessionScope::All => None,
				},
				match scope {
					SessionScope::Current => 10,
					SessionScope::All => 100,
				},
				&mut progress,
			);
			let _ = tx.send(SessionListMsg::Loaded { scope, sessions });
		});
	}

	fn handle_session_list_msg(&mut self, msg: SessionListMsg) {
		let Some(overlay) = self.session_overlay.as_mut() else {
			return;
		};
		match msg {
			SessionListMsg::Progress {
				scope,
				loaded,
				total,
				partial,
			} => {
				if scope == overlay.scope {
					overlay.set_loading_progress(scope, loaded, total);
					if !partial.is_empty() {
						overlay.set_sessions(scope, partial);
					}
				}
			}
			SessionListMsg::Loaded { scope, sessions } => {
				overlay.set_sessions(scope, sessions);
				overlay.set_loading(scope, false);
			}
			SessionListMsg::Status { kind, text } => {
				let ttl = match kind {
					StatusKind::Info => 2000,
					StatusKind::Error => 3000,
				};
				overlay.set_status(kind, text, ttl);
			}
		}
	}

	fn handle_session_overlay_key(&mut self, key: KeyEvent) {
		let action = match self.session_overlay.as_mut() {
			Some(overlay) => overlay.handle_key(&key, &self.keybindings),
			None => return,
		};
		match action {
			SelectorAction::None => {}
			SelectorAction::Cancel => self.session_overlay = None,
			SelectorAction::Resume(path) => {
				self.session_overlay = None;
				let _ = self.session.cmd_tx.send(SessionCommand::SwitchSession {
					path: path.display().to_string(),
				});
				// The switch notice ("session → …") arrives over the event
				// bus and triggers the chat rebuild; a failed switch
				// surfaces its error notice with the selector closed.
			}
			SelectorAction::Delete(path) => {
				let tx = self.session_list_tx.clone();
				tokio::task::spawn_blocking(move || {
					let (kind, text) = match delete_session_file(&path) {
						Ok(message) => (StatusKind::Info, message.to_string()),
						Err(error) => (StatusKind::Error, format!("Failed to delete: {error}")),
					};
					let _ = tx.send(SessionListMsg::Status { kind, text });
				});
				self.reload_sessions();
			}
			SelectorAction::Rename { path, name } => {
				let tx = self.session_list_tx.clone();
				tokio::task::spawn_blocking(move || {
					let (kind, text) =
						match crate::core::session_manager::append_session_info_to_file(
							&path, &name,
						) {
							Ok(()) => (StatusKind::Info, "Session renamed".to_string()),
							Err(error) => (StatusKind::Error, format!("Failed to rename: {error}")),
						};
					let _ = tx.send(SessionListMsg::Status { kind, text });
				});
				self.reload_sessions();
			}
		}
		// A scope toggle with a not-yet-loaded listing requests a load.
		if let Some(overlay) = self.session_overlay.as_ref()
			&& let Some(scope) = overlay.pending_load()
		{
			self.load_sessions(scope);
		}
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
		self.slash_menu = None;
		// Bash passthrough (upstream bash mode): `!cmd` runs outside the
		// agent loop and lands in the transcript; `!!cmd` additionally
		// stays out of the model context.
		if let Some(rest) = line.strip_prefix('!') {
			let (exclude_from_context, command) = match rest.strip_prefix('!') {
				Some(command) => (true, command.trim()),
				None => (false, rest.trim()),
			};
			if !command.is_empty() {
				self.run_passthrough_bash(command, exclude_from_context);
			}
			return;
		}
		if let Some(command) = line.strip_prefix('/') {
			self.handle_slash(command.trim());
		} else {
			self.send_prompt(line);
		}
	}

	/// Send a passthrough bash command; output streams into a dedicated
	/// chat block via the wire-event channel.
	fn run_passthrough_bash(&mut self, command: &str, exclude_from_context: bool) {
		if self.bash_running {
			self.push_notice(
				"A bash command is already running. Press Esc to cancel it first.".into(),
			);
			return;
		}
		self.bash_running = true;
		let _ = self.session.cmd_tx.send(SessionCommand::ExecuteBash {
			command: command.to_string(),
			exclude_from_context,
			id: None,
		});
		self.blocks.push(ChatBlock::Bash {
			command: command.to_string(),
			output: String::new(),
			status: "running".into(),
			rendered: None,
		});
		self.chat.scroll_to_bottom();
	}

	/// Re-evaluate the slash menu against the editor text: opens while the
	/// text is a single `/token`, closes otherwise or on zero matches
	/// (upstream `getSuggestions` returns null → no popup).
	fn update_slash_menu(&mut self) {
		let query = match slash_command_query(self.editor.text()) {
			Some(query) => query,
			None => {
				self.slash_menu = None;
				return;
			}
		};
		if self.slash_menu.is_none() {
			let enable_skill_commands = crate::core::settings_manager::SettingsManager::load(Some(
				std::path::Path::new(&self.cwd),
			))
			.enable_skill_commands();
			self.slash_menu = Some(SlashMenu::new(slash_menu_items(
				self.session.resources(),
				enable_skill_commands,
				&self.session.extension_commands(),
			)));
		}
		let menu = self.slash_menu.as_mut().unwrap();
		menu.list.set_filter(query);
		if menu.list.filtered().is_empty() {
			self.slash_menu = None;
		}
	}

	/// Keys the open menu consumes. Returns `false` for everything else so
	/// typing continues in the editor (`update_slash_menu` then refilters).
	fn handle_slash_menu_key(&mut self, key: KeyEvent) -> bool {
		match key.code {
			KeyCode::Esc => {
				// custom-editor.ts: escape cancels the autocomplete first and
				// must not abort the running turn.
				self.slash_menu = None;
				true
			}
			KeyCode::Tab | KeyCode::Enter => {
				// Upstream `applyCompletion`'s slash branch falls through to
				// submit: Enter completes the command AND runs it. Tab only
				// completes (`tui.input.tab` upstream).
				self.apply_slash_menu_selection();
				if key.code == KeyCode::Enter {
					let line = self.editor.text().trim().to_string();
					if !line.is_empty() {
						self.editor.set_text("");
						self.handle_submit(line);
					}
				}
				true
			}
			KeyCode::Up | KeyCode::Down => {
				let action = match self.slash_menu.as_mut() {
					Some(menu) => menu.list.handle_key_with(key, &self.keybindings),
					None => return true,
				};
				!matches!(action, SelectAction::Ignored)
			}
			_ => false,
		}
	}

	/// Complete the highlighted command into the editor (`/name `), upstream
	/// `applyCompletion`'s slash branch. With no match under the cursor the
	/// menu stays open (pi behavior).
	fn apply_slash_menu_selection(&mut self) {
		let Some(menu) = self.slash_menu.as_ref() else {
			return;
		};
		let Some(item) = menu.list.selected_item() else {
			return;
		};
		let name = item.value.clone();
		let completed = apply_slash_completion(&name);
		self.editor.set_text(completed);
		// The trailing space disqualifies the menu; refresh to be exact.
		self.update_slash_menu();
	}

	/// `/trust` (upstream `showTrustSelector` + `TrustSelectorComponent`):
	/// show the cwd, the saved decision and the effective session state, then
	/// save Trusted / Untrusted into the trust store for future sessions. The
	/// parent-folder option mirrors upstream's `Trust parent folder` entry;
	/// upstream's "session only" decisions have no port counterpart (no
	/// in-memory trust override).
	fn handle_trust_command(&mut self) {
		let cwd = std::path::PathBuf::from(&self.cwd);
		let saved = crate::core::trust::saved_entry(&cwd);
		// Upstream `formatDecision`: "none", "trusted (path)", or
		// "… (inherited from <path>)" when the decision was recorded for an
		// ancestor.
		let saved_text = match &saved {
			None => "none".to_string(),
			Some((path, decision)) => {
				let label = if *decision { "trusted" } else { "untrusted" };
				if path != &cwd {
					format!("{label} (inherited from {})", path.display())
				} else {
					format!("{label} ({})", path.display())
				}
			}
		};
		let current = crate::core::trust::is_trusted(&cwd);
		let header = vec![
			"Project trust".to_string(),
			cwd.display().to_string(),
			String::new(),
			format!("Saved decision: {saved_text}"),
			format!(
				"Current session: {}",
				if current { "trusted" } else { "untrusted" }
			),
		];
		let mut items = vec![SelectItem {
			value: "trust".into(),
			label: "Trust".into(),
			description: Some("save this project as trusted".into()),
		}];
		// Upstream offers the parent folder when there is one; its update
		// also clears the project's own entry so the ancestor decides.
		let parent = cwd.parent().map(std::path::Path::to_path_buf);
		if let Some(parent) = &parent {
			items.push(SelectItem {
				value: "trust-parent".into(),
				label: format!("Trust parent folder ({})", parent.display()),
				description: Some("save the parent folder as trusted".into()),
			});
		}
		items.push(SelectItem {
			value: "untrust".into(),
			label: "Do not trust".into(),
			description: Some("save this project as untrusted".into()),
		});
		let mut list = SelectList::new(items, 4);
		// The saved decision is preselected (upstream `findIndex` over the
		// matching `savedPath`, falling back to the first option when no
		// option matches — e.g. an inherited untrusted decision).
		let preselect = match &saved {
			Some((path, true)) if path == &cwd => 0,
			Some((path, true)) if parent.is_some() => 1,
			Some((path, false)) if path == &cwd => list.len() - 1,
			_ => 0,
		};
		list.set_selected(preselect);
		list.set_no_match_message("No trust options");
		self.trust_overlay = Some((list, TrustOverlayInfo { cwd, header }));
	}

	fn handle_trust_overlay_key(&mut self, key: KeyEvent) {
		let action = match self.trust_overlay.as_mut() {
			Some((list, _)) => list.handle_key_with(key, &self.keybindings),
			None => return,
		};
		let choice = match action {
			SelectAction::Select(index) => self
				.trust_overlay
				.as_ref()
				.and_then(|(list, _)| list.filtered().get(index).map(|item| item.value.clone())),
			SelectAction::Cancel => Some("cancel".into()),
			SelectAction::Handled | SelectAction::Ignored => None,
		};
		let Some(choice) = choice else {
			return;
		};
		let (mut list, info) = self.trust_overlay.take().unwrap();
		let cwd = info.cwd.clone();
		let result = match choice.as_str() {
			"trust" => crate::core::trust::set(&cwd, true).map(|_| "trusted"),
			"trust-parent" => {
				// Upstream `updates`: parent true + the project entry nulled
				// so the ancestor decision applies to it.
				let parent = cwd.parent().map(std::path::Path::to_path_buf);
				match parent {
					Some(parent) => crate::core::trust::set(&parent, true)
						.and_then(|()| crate::core::trust::clear(&cwd))
						.map(|_| "trusted"),
					None => Err(std::io::Error::other("no parent folder")),
				}
			}
			"untrust" => crate::core::trust::set(&cwd, false).map(|_| "untrusted"),
			_ => {
				// Esc: reopen nothing, just close.
				return;
			}
		};
		match result {
			Ok(decision) => self.push_notice(format!(
				"Saved trust decision: {decision}. Restart {} for this to take effect.",
				crate::config::APP_NAME
			)),
			Err(error) => {
				// Keep the selector open on a write failure so the decision
				// can be retried (upstream would surface the throw).
				self.push_notice(format!("Could not save trust decision: {error}"));
				list.set_mark_value(None);
				self.trust_overlay = Some((list, info));
			}
		}
	}

	/// `/hotkeys` (upstream `handleHotkeysCommand`): the resolved keybindings
	/// as a markdown table in the upstream Navigation / Editing / Other
	/// sections, rendered as a bordered markdown block. Upstream's extension
	/// shortcut section has no port counterpart (no extension runner).
	fn handle_hotkeys_command(&mut self) {
		let markdown = hotkeys_markdown(&self.keybindings);
		self.push_doc_block("Keyboard Shortcuts", markdown);
	}

	/// `/changelog` (upstream `handleChangelogCommand`): the keepachangelog
	/// sections newest-first, rendered as a bordered "What's New" markdown
	/// block (`DynamicBorder` + `Markdown` upstream).
	fn handle_changelog_command(&mut self) {
		let markdown = crate::core::changelog::changelog_markdown();
		self.push_doc_block("What's New", markdown);
	}

	/// Add a [`ChatBlock::Doc`] — a bordered markdown document block.
	fn push_doc_block(&mut self, title: &str, text: String) {
		self.blocks.push(ChatBlock::Doc {
			title: title.to_string(),
			text,
			rendered: None,
		});
		self.chat.scroll_to_bottom();
	}

	/// Drop every block's rendered cache (theme/keybindings changed).
	fn invalidate_rendered_blocks(&mut self) {
		for block in &mut self.blocks {
			match block {
				ChatBlock::Assistant { rendered, .. }
				| ChatBlock::Tool { rendered, .. }
				| ChatBlock::Bash { rendered, .. }
				| ChatBlock::Doc { rendered, .. } => *rendered = None,
				ChatBlock::User(_) | ChatBlock::Notice(_) => {}
			}
		}
	}

	// -- /settings (upstream showSettingsSelector) ------------------------------

	/// Open the settings selector over the effective values (upstream feeds
	/// the component from `settingsManager` getters; pi-rs loads both layers
	/// and shows `effective()`).
	fn open_settings_selector(&mut self) {
		let effective = crate::core::settings_manager::SettingsManager::load(Some(
			std::path::Path::new(&self.cwd),
		))
		.effective();
		let entries = settings_entries();
		let items: Vec<SelectItem> = entries
			.iter()
			.map(|entry| SelectItem {
				value: entry.key.to_string(),
				label: format!("{}: {}", entry.key, setting_value(&effective, entry.key)),
				description: Some(
					match entry.editor {
						SettingEditor::Cycle(_) => "enter cycles the value",
						SettingEditor::Input => "enter edits the value",
					}
					.to_string(),
				),
			})
			.collect();
		let mut list = SelectList::new(items, 12);
		list.set_no_match_message("No matching settings");
		self.settings_overlay = Some(SettingsOverlay { list, entries });
	}

	/// Keys for the settings selector: type-to-filter, Enter cycles or opens
	/// the value editor, Esc closes. Every change writes the global layer
	/// immediately (upstream's setters persist per callback).
	fn handle_settings_overlay_key(&mut self, key: KeyEvent) {
		if !key.modifiers.contains(KeyModifiers::CONTROL)
			&& !key.modifiers.contains(KeyModifiers::ALT)
		{
			let Some(overlay) = self.settings_overlay.as_mut() else {
				return;
			};
			// The list's fuzzy filter doubles as the search line (a fresh
			// query re-anchors the selection at the top row).
			match key.code {
				KeyCode::Char(c) => {
					let filter = format!("{}{c}", overlay.list.filter());
					overlay.list.set_filter(filter);
					return;
				}
				KeyCode::Backspace => {
					let mut filter = overlay.list.filter().to_string();
					filter.pop();
					overlay.list.set_filter(filter);
					return;
				}
				_ => {}
			}
		}
		let action = match self.settings_overlay.as_mut() {
			Some(overlay) => overlay.list.handle_key_with(key, &self.keybindings),
			None => return,
		};
		match action {
			SelectAction::Select(index) => {
				let Some((key_id, editor)) = self.settings_overlay.as_ref().and_then(|overlay| {
					overlay.list.filtered().get(index).and_then(|item| {
						overlay
							.entries
							.iter()
							.find(|entry| entry.key == item.value)
							.map(|entry| (entry.key.to_string(), entry.editor.clone()))
					})
				}) else {
					return;
				};
				match editor {
					SettingEditor::Cycle(values) => {
						let effective = crate::core::settings_manager::SettingsManager::load(Some(
							std::path::Path::new(&self.cwd),
						))
						.effective();
						let current = setting_value(&effective, &key_id);
						let position = values.iter().position(|value| value == &current);
						let next = match position {
							Some(position) => values[(position + 1) % values.len()].clone(),
							None => values.first().cloned().unwrap_or_default(),
						};
						self.save_setting(&key_id, &next);
					}
					SettingEditor::Input => {
						self.dialog = Some(Dialog::SettingInput {
							key: key_id,
							value: String::new(),
						});
					}
				}
			}
			SelectAction::Cancel => self.settings_overlay = None,
			SelectAction::Handled | SelectAction::Ignored => {}
		}
	}

	/// Persist one settings override to the global layer (upstream
	/// `applyOverrides` writes global), then hot-apply the instant keys.
	fn save_setting(&mut self, key: &str, raw: &str) {
		let overrides = match setting_override(key, raw) {
			Ok(overrides) => overrides,
			Err(error) => {
				self.push_notice(error);
				return;
			}
		};
		let mut manager = crate::core::settings_manager::SettingsManager::load(Some(
			std::path::Path::new(&self.cwd),
		));
		manager.set_overrides(&overrides);
		if let Err(error) = manager.save() {
			self.push_notice(format!("Could not save settings: {error}"));
			return;
		}
		let instant = match key {
			"defaultThinkingLevel" => {
				// Hot-apply: the session's thinking level follows the saved
				// default immediately.
				if let Some(level) = parse_setting_thinking_level(raw) {
					let _ = self
						.session
						.cmd_tx
						.send(SessionCommand::SetThinking { level });
				}
				true
			}
			"theme" => {
				// Hot-apply: re-resolve the theme by name (built-ins and the
				// user themes dir, like startup).
				let ui = crate::core::ui_config::load(Some(raw), false);
				self.theme = ui.theme;
				self.invalidate_rendered_blocks();
				true
			}
			"treeFilterMode" => {
				// Read fresh on every /tree open, so it is already live.
				true
			}
			_ => false,
		};
		self.push_notice(if instant {
			format!("{key} saved: {raw}")
		} else {
			format!("{key} saved (takes effect on restart)")
		});
		// Refresh the row labels from the saved manager.
		if let Some(overlay) = self.settings_overlay.as_mut() {
			let effective = manager.effective();
			let items: Vec<SelectItem> = overlay
				.entries
				.iter()
				.map(|entry| SelectItem {
					value: entry.key.to_string(),
					label: format!("{}: {}", entry.key, setting_value(&effective, entry.key)),
					description: Some(
						match entry.editor {
							SettingEditor::Cycle(_) => "enter cycles the value",
							SettingEditor::Input => "enter edits the value",
						}
						.to_string(),
					),
				})
				.collect();
			overlay.list.set_items(items);
		}
	}

	// -- /scoped-models (upstream showModelsSelector) ---------------------------

	/// Open the multi-select over the full catalog with the active scope
	/// pre-checked and the default model marked.
	fn open_scoped_models_selector(&mut self) {
		let registry = self.registry.lock().unwrap();
		let all = registry.all_models().to_vec();
		let scoped = registry.models().to_vec();
		let default_key = registry.default_model().key();
		drop(registry);
		let checked: std::collections::BTreeSet<String> = if scoped.len() < all.len() {
			scoped.iter().map(|model| model.key()).collect()
		} else {
			std::collections::BTreeSet::new()
		};
		self.scoped_overlay = Some(ScopedModelsOverlay::new(&all, checked, &default_key));
	}

	/// Keys: Enter toggles the highlighted model, Tab applies the scope
	/// (upstream: confirm toggles and `app.models.save` persists — pi-rs
	/// has no `enabledModels` setting, so applying is session-only), Esc
	/// cancels, typing filters.
	fn handle_scoped_models_key(&mut self, key: KeyEvent) {
		if !key.modifiers.contains(KeyModifiers::CONTROL)
			&& !key.modifiers.contains(KeyModifiers::ALT)
		{
			let Some(overlay) = self.scoped_overlay.as_mut() else {
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
		// Tab (tui.input.tab) applies without toggling; Enter toggles.
		if self.keybindings.matches("tui.input.tab", &key) {
			self.apply_scoped_models();
			return;
		}
		let action = match self.scoped_overlay.as_mut() {
			Some(overlay) => overlay.list.handle_key_with(key, &self.keybindings),
			None => return,
		};
		match action {
			SelectAction::Select(index) => {
				let Some(overlay) = self.scoped_overlay.as_mut() else {
					return;
				};
				let filtered = overlay.list.filtered();
				let Some(item) = filtered.get(index) else {
					return;
				};
				let model_key = item.value.clone();
				drop(filtered);
				if overlay.checked.contains(&model_key) {
					overlay.checked.remove(&model_key);
				} else {
					overlay.checked.insert(model_key);
				}
				overlay.refresh_items();
			}
			SelectAction::Cancel => self.scoped_overlay = None,
			SelectAction::Handled | SelectAction::Ignored => {}
		}
	}

	/// Apply the checked set as the model scope (empty set clears — upstream
	/// treats an empty scopedModels list as "no scoping").
	fn apply_scoped_models(&mut self) {
		let Some(overlay) = self.scoped_overlay.take() else {
			return;
		};
		let all = self.registry.lock().unwrap().all_models().to_vec();
		let scoped = scope_models_for(&all, &overlay.checked);
		{
			let mut registry = self.registry.lock().unwrap();
			registry.set_model_scope(scoped.to_vec());
			self.models = registry.models().to_vec();
		}
		if scoped.is_empty() {
			self.push_notice("Model scope cleared — all models are available".into());
		} else {
			// Upstream persists `enabledModels` on save; pi-rs keeps the
			// scope session-only (no such setting yet).
			self.push_notice(format!(
				"Model scope set: {} models (session only; cycling and /model see these)",
				scoped.len()
			));
		}
	}

	// -- /reload (upstream handleReloadCommand) ----------------------------------

	/// `/reload`: rebuild the runtime — UI config (keybindings, theme),
	/// model registry, and the session itself via the replacement machinery
	/// re-opening the same session path. Extensions/skills/prompts have no
	/// pi-rs counterpart, so the status text keeps upstream's wording while
	/// those reloads are no-ops here (declared difference).
	fn handle_reload_command(&mut self) {
		if self.working {
			self.push_notice("Wait for the current response to finish before reloading.".into());
			return;
		}
		if self.session.compacting.load(Ordering::Relaxed) {
			// The flag also covers tree navigation and mid-run compaction
			// guards (upstream checks isCompacting).
			self.push_notice("Wait for compaction to finish before reloading.".into());
			return;
		}
		self.push_notice(
			"Reloading keybindings, extensions, skills, prompts, themes, and context files..."
				.into(),
		);
		// UI config: keybindings.json + the settings theme (upstream
		// keybindings.reload() + themeController.applyFromSettings()).
		let settings = crate::core::settings_manager::SettingsManager::load(Some(
			std::path::Path::new(&self.cwd),
		));
		let effective = settings.effective();
		let ui = crate::core::ui_config::load(effective.theme.as_deref(), false);
		self.keybindings = ui.keybindings;
		for notice in ui.notices {
			self.push_notice(notice);
		}
		self.theme = ui.theme;
		self.external_editor_cmd =
			crate::core::ui_config::resolve_external_editor(effective.external_editor.as_deref());
		// Model catalog (upstream reloads the resource loader; pi-rs re-reads
		// models.json and re-resolves provider auth).
		{
			let mut registry = self.registry.lock().unwrap();
			registry.reload(effective.default_model.as_deref());
			self.models = registry.models().to_vec();
		}
		// Session rebuild: the replacement machinery re-opens the same
		// session path, which rebuilds cwd-bound tools and the system prompt
		// (apply_replacement) — the closest port of upstream's session
		// reload. In-memory sessions only rebuild the view.
		match self.session.current_session_file() {
			Some(path) => {
				let path = path.to_string_lossy().into_owned();
				self.begin_replacement_labeled(
					RpcReplacement::Switch { path },
					ReplacementKind::Reload,
					"Reload failed",
					None,
				);
			}
			None => {
				self.restore_transcript();
				self.push_notice("Reloaded".into());
			}
		}
	}

	/// `/session` (upstream `handleSessionCommand`): name, file, id, message
	/// counts, and the token/cost totals with the cached/uncached split. The
	/// cache-warming and per-model breakdown sections have no port
	/// counterpart.
	fn handle_session_command(&mut self) {
		let (session_file, log_stats) = {
			let guard = self.session.log.lock().unwrap();
			(
				self.session.current_session_file(),
				crate::core::agent_session::compute_session_stats(&guard.session, None),
			)
		};
		let model = self.session.live_agent().model();
		let mut info = String::from("Session Info\n\n");
		if let Some(name) = self.session.session_name() {
			info.push_str(&format!("Name: {name}\n"));
		}
		info.push_str(&format!(
			"File: {}\n",
			session_file
				.as_ref()
				.map(|path| path.display().to_string())
				.unwrap_or_else(|| "In-memory".into())
		));
		info.push_str(&format!("ID: {}\n\n", log_stats.session_id));
		info.push_str("Messages\n");
		info.push_str(&format!("Total: {}\n", log_stats.total_messages));
		info.push_str(&format!("User: {}\n", log_stats.user_messages));
		info.push_str(&format!("Assistant: {}\n", log_stats.assistant_messages));
		info.push_str(&format!(
			"Tools: {} calls, {} results\n\n",
			log_stats.tool_calls, log_stats.tool_results
		));
		info.push_str("Tokens\n");
		let prompt_tokens = log_stats.input + log_stats.cache_read + log_stats.cache_write;
		info.push_str(&format!("Input: {prompt_tokens}\n"));
		if prompt_tokens > 0 && (log_stats.cache_read > 0 || log_stats.cache_write > 0) {
			info.push_str(&format!(
				"  Cached: {} ({:.1}%)\n",
				log_stats.cache_read,
				log_stats.cache_read as f64 / prompt_tokens as f64 * 100.0
			));
			info.push_str(&format!(
				"  Uncached: {}\n",
				log_stats.input + log_stats.cache_write
			));
		}
		info.push_str(&format!("Output: {}\n", log_stats.output));
		let total =
			log_stats.input + log_stats.output + log_stats.cache_read + log_stats.cache_write;
		info.push_str(&format!("Total: {total}\n"));
		info.push_str(&format!("\nModel: {}", model.key()));
		self.push_notice(info);
	}

	/// `/name [name]` (upstream `handleNameCommand`): show the current
	/// display name, or set one. The sanitized name arrives through the
	/// session task; a mismatch surfaces as a warning.
	fn handle_name_command(&mut self, args: &str) {
		let name = args.trim();
		if name.is_empty() {
			match self.session.session_name() {
				Some(current) => self.push_notice(format!("Session name: {current}")),
				None => self.push_notice("Usage: /name <name>".into()),
			}
			return;
		}
		let _ = self.session.cmd_tx.send(SessionCommand::SetSessionName {
			name: name.to_string(),
		});
		// The authoritative sanitized name lands asynchronously; echo the
		// requested one (upstream reads back synchronously and warns on
		// normalization — the port's notice would race the session task).
		self.push_notice(format!("Session name set: {name}"));
	}

	/// ctrl+p / alt+p (upstream `cycleModel`, available-models variant): move
	/// ±1 through the registry list with wraparound. The scoped-models
	/// variant has no port counterpart.
	fn cycle_model(&mut self, step: isize) {
		if self.models.len() < 2 {
			self.push_notice("Only one model available".into());
			return;
		}
		let current = self.session.live_agent().model().key();
		let count = self.models.len() as isize;
		let index = self
			.models
			.iter()
			.position(|model| model.key() == current)
			.map(|index| ((index as isize + step).rem_euclid(count)) as usize)
			.unwrap_or(0);
		let next = self.models[index].key();
		let _ = self
			.session
			.cmd_tx
			.send(SessionCommand::SetModel { query: next });
	}

	/// `/copy` / ctrl+x (upstream `handleCopyCommand`): copy the last
	/// assistant message to the clipboard. The alt-screen selection branch
	/// has no port counterpart (no selection API).
	fn handle_copy_command(&mut self) {
		let text = self
			.session
			.live_agent()
			.snapshot()
			.messages
			.iter()
			.rev()
			.find_map(|message| match message {
				Message::Assistant(assistant) => {
					let text = assistant.text();
					(!text.is_empty()).then_some(text)
				}
				_ => None,
			});
		let Some(text) = text else {
			self.push_notice("No agent messages to copy yet.".into());
			return;
		};
		match crate::core::clipboard::copy_to_clipboard(&text) {
			Ok(()) => self.push_notice("Copied last agent message to clipboard".into()),
			Err(error) => self.push_notice(error),
		}
	}

	/// `/export [path]` (upstream `handleExportCommand`): `.jsonl` writes the
	/// current branch, anything else HTML; no argument takes the default
	/// path. In-memory sessions only export JSONL.
	fn handle_export_command(&mut self, args: &str) {
		let arg = args.trim();
		let arg = match (arg.chars().next(), arg.chars().last()) {
			(Some(q), Some(last)) if (q == '"' || q == '\'') && q == last && arg.len() >= 2 => {
				&arg[1..arg.len() - 1]
			}
			_ => arg,
		};
		let outcome: anyhow::Result<std::path::PathBuf> = (|| {
			if arg.ends_with(".jsonl") {
				let log = self.session.log.lock().unwrap();
				let path = crate::core::session_export::export_session_to_jsonl(
					&log.session,
					(!arg.is_empty()).then_some(std::path::Path::new(arg)),
					std::path::Path::new(&self.cwd),
				)?;
				return Ok(path);
			}
			let session_dir = self
				.session
				.log
				.lock()
				.unwrap()
				.session
				.session_dir()
				.map(std::path::Path::to_path_buf);
			let Some(session_dir) = session_dir else {
				anyhow::bail!("Cannot export in-memory session to HTML");
			};
			crate::core::export_html::write_session_html(
				&session_dir,
				(!arg.is_empty()).then_some(std::path::Path::new(arg)),
			)
		})();
		match outcome {
			Ok(path) => self.push_notice(format!("Session exported to: {}", path.display())),
			Err(error) => self.push_notice(format!("Failed to export session: {error}")),
		}
	}

	fn send_prompt(&mut self, text: String) {
		let images = std::mem::take(&mut self.pending_images);
		let _ = self
			.session
			.cmd_tx
			.send(SessionCommand::Prompt { text, images });
		self.chat.scroll_to_bottom();
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
				// Dynamic resource commands (templates, skills) and the
				// registered extension commands join the listing like
				// upstream's /help reads the command registry.
				let enable_skill_commands = crate::core::settings_manager::SettingsManager::load(
					Some(std::path::Path::new(&self.cwd)),
				)
				.enable_skill_commands();
				for command in crate::core::slash_commands::dynamic_slash_commands(
					&self.session.resources().prompt_templates,
					&self.session.resources().skills,
					enable_skill_commands,
					&self.session.extension_commands(),
				) {
					text.push_str(&format!(
						"  /{:<10} {}\n",
						command.name, command.description
					));
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
			"model" => {
				if args.is_empty() {
					let current = self.session.live_agent().model().key();
					self.overlay = Some(ModelOverlay::new(&self.models, &current));
				} else {
					let _ = self.session.cmd_tx.send(SessionCommand::SetModel {
						query: args.to_string(),
					});
				}
			}
			"resume" => {
				// `/resume` opens the session selector; `/resume <id|path>`
				// resumes that session directly (upstream `--session`).
				if args.is_empty() {
					self.open_session_selector();
				} else {
					self.resume_session(args);
				}
			}
			"copy" => self.handle_copy_command(),
			"name" => self.handle_name_command(args),
			"session" => self.handle_session_command(),
			"hotkeys" => self.handle_hotkeys_command(),
			"settings" => self.open_settings_selector(),
			"changelog" => self.handle_changelog_command(),
			"reload" => self.handle_reload_command(),
			"scoped-models" => self.open_scoped_models_selector(),
			"fork" => {
				if args.is_empty() {
					self.open_fork_selector();
				} else {
					self.fork_by_index(args);
				}
			}
			"trust" => self.handle_trust_command(),
			"new" => {
				// Upstream `handleClearCommand`: a fresh session (queued
				// prompts are discarded by the replacement); the "session → "
				// notice rebuilds the chat view.
				let _ = self.session.cmd_tx.send(SessionCommand::NewSession {
					parent_session: None,
				});
				self.push_notice("New session started".into());
			}
			"export" => self.handle_export_command(args),
			"thinking" => {
				// Upstream `handleThinkingCommand`: an argument sets that
				// level directly; bare `/thinking` cycles.
				if args.is_empty() {
					let _ = self.session.cmd_tx.send(SessionCommand::CycleThinking);
					return;
				}
				let levels = thinking_levels_for_model(&self.session.live_agent().model());
				let requested = args.trim().to_lowercase();
				let wire = |level: &ThinkingLevel| match level {
					ThinkingLevel::Off => "off",
					ThinkingLevel::Minimal => "minimal",
					ThinkingLevel::Low => "low",
					ThinkingLevel::Medium => "medium",
					ThinkingLevel::High => "high",
					ThinkingLevel::Xhigh => "xhigh",
					ThinkingLevel::Max => "max",
				};
				match levels.iter().find(|level| wire(level) == requested) {
					Some(level) => {
						let _ = self
							.session
							.cmd_tx
							.send(SessionCommand::SetThinking { level: *level });
					}
					None => {
						let names = levels.iter().map(wire).collect::<Vec<_>>().join(", ");
						self.push_notice(format!(
							"Unknown thinking level \"{requested}\". Available levels: {names}."
						));
					}
				}
			}
			"tools" => {
				let names = self.session.live_agent().tool_names().join(", ");
				self.push_notice(format!("active tools: {names}"));
			}
			"prompt" => {
				let prompt = self.session.live_agent().snapshot().system_prompt;
				let preview: Vec<&str> = prompt.lines().take(30).collect();
				self.push_notice(format!(
					"system prompt (first 30 lines):\n{}\n…",
					preview.join("\n")
				));
			}
			"compact" => {
				let instructions = (!args.is_empty()).then(|| args.to_string());
				let _ = self
					.session
					.cmd_tx
					.send(SessionCommand::Compact { instructions });
			}
			"sessions" => {
				// Upstream has no /sessions slash command (the selector covers
				// it); this pi-rs listing uses the SessionInfo pipeline.
				let sessions = crate::core::session_manager::list_sessions_in(
					&crate::config::sessions_dir(),
					None,
				);
				if sessions.is_empty() {
					self.push_notice("no saved sessions yet".into());
				} else {
					let mut text = String::from("Recent sessions:\n");
					for session in sessions.iter().take(10) {
						let age = {
							let diff = pi_ai::types::now_ms().saturating_sub(session.modified_ms);
							let days = diff / 86_400_000;
							if diff < 3_600_000 {
								format!("{}m", diff / 60_000)
							} else if days < 1 {
								format!("{}h", diff / 3_600_000)
							} else if days < 30 {
								format!("{}d", days)
							} else {
								format!("{}mo", days / 30)
							}
						};
						let title = session
							.name
							.clone()
							.unwrap_or_else(|| session.first_message.clone());
						text.push_str(&format!(
							"  {:>4}  {}  ({} msgs)\n",
							age,
							title.chars().take(70).collect::<String>(),
							session.message_count
						));
					}
					self.push_notice(text);
				}
			}
			other => {
				// Extension commands execute through the session task
				// immediately — even during a run, like upstream
				// `prompt()`'s extension-command branch. The response
				// channel is dropped: outcomes surface as notices
				// (`ui.notify` → chat notice) from the execution itself.
				if self
					.session
					.extension_commands()
					.iter()
					.any(|(name, _)| name == other)
				{
					let (tx, _rx) = tokio::sync::oneshot::channel();
					let _ = self.session.cmd_tx.send(SessionCommand::ExtensionCommand {
						name: other.to_string(),
						args: args.to_string(),
						response: tx,
					});
					return;
				}
				// Template (`/<name>`) and skill (`/skill:<name>`) commands
				// ride the prompt expansion path (upstream `prompt()` expands
				// them; an unknown name passes through as an ordinary
				// message, so it is only "known" when a resource matches).
				let resources = self.session.resources();
				let is_template = resources
					.prompt_templates
					.iter()
					.any(|template| template.name == other);
				let is_skill = other
					.strip_prefix("skill:")
					.is_some_and(|name| resources.skills.iter().any(|skill| skill.name == name));
				if is_template || is_skill {
					self.send_prompt(format!("/{command}"));
				} else {
					self.push_notice(format!("unknown command /{other} — try /help"));
				}
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

	/// `/fork` selector — the user messages a fork can start from (upstream
	/// `showUserMessageSelector`). Confirming forks before the message and
	/// prefills the editor with its text (upstream `editor.setText`).
	fn open_fork_selector(&mut self) {
		let messages = self.fork_messages();
		if messages.is_empty() {
			self.push_notice("No messages to fork from".into());
		} else {
			self.overlay = Some(ModelOverlay::fork(messages));
		}
	}

	/// `/fork <n>` — fork before the n-th user message (1-based, pi-rs
	/// extension). The fork runs through the replacement machinery, so the
	/// editor is prefilled with the selected message like the selector path.
	fn fork_by_index(&mut self, args: &str) {
		let messages = self.fork_messages();
		let index: usize = match args.trim().parse::<usize>() {
			Ok(index) if index >= 1 && index <= messages.len() => index - 1,
			_ => {
				self.push_notice(format!(
					"No user message {} — /fork lists the valid indexes.",
					args.trim()
				));
				return;
			}
		};
		let (entry_id, _) = messages[index].clone();
		self.begin_replacement_labeled(
			RpcReplacement::Fork { entry_id },
			ReplacementKind::Fork,
			"Session switch failed",
			None,
		);
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
			Dialog::SettingInput {
				key: key_id,
				mut value,
			} => match key.code {
				// Esc returns to the settings selector (upstream's submenu
				// back navigation).
				KeyCode::Esc => self.dialog = None,
				KeyCode::Enter => {
					self.dialog = None;
					let value = value.trim().to_string();
					self.save_setting(&key_id, &value);
				}
				KeyCode::Backspace => {
					value.pop();
					self.dialog = Some(Dialog::SettingInput { key: key_id, value });
				}
				KeyCode::Char(c)
					if !key
						.modifiers
						.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
				{
					value.push(c);
					self.dialog = Some(Dialog::SettingInput { key: key_id, value });
				}
				_ => {}
			},
		}
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
					ReplacementKind::Reload => {
						self.push_notice("Reloaded".into());
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
			SessionEvent::Notice(text) => {
				// The replacement itself restores the transcript (the
				// `SessionReplaced` event); the notice only reports the new
				// destination ("session → …").
				self.push_notice(text);
			}
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
			SessionEvent::ExtensionDialog(request) => self.open_extension_dialog(request),
			// ui.setEditorText (upstream interactive-mode.ts:2656): plain
			// editor buffer replace — no paste handling on this path.
			SessionEvent::ExtensionEditorText(text) => self.editor.set_text(text),
			// ui.setStatus (footer.ts:146-151 + 289-299): a single slot here
			// because the host shim drops the upstream `key`; missing text
			// clears, empty text keeps an (empty) third footer line — same
			// as upstream's `size > 0` gate. Footer height grows in draw().
			SessionEvent::ExtensionStatus(text) => self.extension_status = text,
			// ui.setTitle (interactive-mode.ts:2653 → terminal.setTitle,
			// terminal.ts:592-594): OSC-0 terminal window title.
			SessionEvent::ExtensionTitle(title) => {
				let _ = crossterm::execute!(
					std::io::stdout(),
					crossterm::terminal::SetTitle(title.as_str())
				);
			}
		}
	}

	// -- extension dialogs -------------------------------------------------------

	/// Open an extension dialog, or queue it when one is already on screen
	/// (the handlers park on the bridge registry until their turn).
	fn open_extension_dialog(&mut self, request: crate::core::extensions::ExtensionDialogRequest) {
		if self.extension_dialog.is_some() {
			self.pending_extension_dialogs.push_back(request);
			return;
		}
		self.extension_dialog = Some(ExtensionDialogState::new(request));
	}

	/// Answer the open dialog through the bridge registry and surface the
	/// next queued request. A missing bridge drops the answer (the waiting
	/// handler resolves with the no-UI default when its receiver drops).
	fn answer_extension_dialog(&mut self, value: serde_json::Value) {
		if let Some(state) = self.extension_dialog.take()
			&& let Some(bridge) = self.session.resources().extensions.as_ref()
		{
			bridge.answer_dialog(state.request.id, value);
		}
		if let Some(next) = self.pending_extension_dialogs.pop_front() {
			self.extension_dialog = Some(ExtensionDialogState::new(next));
		}
	}

	fn handle_extension_dialog_key(&mut self, key: KeyEvent) {
		let Some(state) = self.extension_dialog.as_mut() else {
			return;
		};
		match &state.request.kind {
			crate::core::extensions::ExtensionDialogKind::Select { options, .. } => {
				match key.code {
					KeyCode::Up => {
						state.cursor = state.cursor.saturating_sub(1);
					}
					KeyCode::Down => {
						state.cursor = (state.cursor + 1).min(options.len().saturating_sub(1));
					}
					KeyCode::Enter => {
						let value = options
							.get(state.cursor)
							.map(|option| serde_json::Value::String(option.clone()))
							.unwrap_or(serde_json::Value::Null);
						self.answer_extension_dialog(value);
					}
					KeyCode::Esc => self.answer_extension_dialog(serde_json::Value::Null),
					_ => {}
				}
			}
			crate::core::extensions::ExtensionDialogKind::Confirm { .. } => match key.code {
				KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
					self.answer_extension_dialog(serde_json::Value::Bool(true));
				}
				KeyCode::Char('n' | 'N') | KeyCode::Esc => {
					self.answer_extension_dialog(serde_json::Value::Bool(false));
				}
				_ => {}
			},
			crate::core::extensions::ExtensionDialogKind::Input { .. } => match key.code {
				KeyCode::Esc => self.answer_extension_dialog(serde_json::Value::Null),
				KeyCode::Enter => {
					let value = std::mem::take(&mut state.value);
					self.answer_extension_dialog(serde_json::Value::String(value));
				}
				KeyCode::Backspace => {
					state.value.pop();
				}
				KeyCode::Char(c)
					if !key
						.modifiers
						.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
				{
					state.value.push(c);
				}
				_ => {}
			},
		}
	}

	/// Passthrough bash stream: append deltas to the live Bash block and
	/// finalize on done (output, exit code, cancellation, spill note).
	fn handle_wire_event(&mut self, wire: WireEvent) {
		match wire {
			WireEvent::BashExecutionUpdate { delta, .. } => {
				if let Some(ChatBlock::Bash {
					output, rendered, ..
				}) = self
					.blocks
					.iter_mut()
					.rev()
					.find(|block| matches!(block, ChatBlock::Bash { .. }))
				{
					output.push_str(&delta);
					*rendered = None;
				}
			}
			WireEvent::BashExecutionDone { result, .. } => {
				self.bash_running = false;
				if let Some(ChatBlock::Bash {
					output,
					status,
					rendered,
					..
				}) = self
					.blocks
					.iter_mut()
					.rev()
					.find(|block| matches!(block, ChatBlock::Bash { .. }))
				{
					*output = result.output;
					*status = match (result.exit_code, result.cancelled) {
						(_, true) => "cancelled".into(),
						(Some(0), false) => "done".into(),
						(Some(code), false) => format!("exit {code}"),
						(None, false) => "done".into(),
					};
					if result.truncated {
						status.push_str(" (truncated — full output on disk)");
					}
					*rendered = None;
				}
				self.chat.scroll_to_bottom();
			}
			WireEvent::SessionInfoChanged { name } => {
				// A session rename (upstream `session_info_changed`) surfaces
				// as a chat notice.
				self.push_notice(format!(
					"Session name: {}",
					name.as_deref().unwrap_or("(none)")
				));
			}
			WireEvent::AutoRetryStart { .. } | WireEvent::AutoRetryEnd { .. } => {}
		}
	}

	fn handle_agent_event(&mut self, event: AgentEvent) {
		match event {
			AgentEvent::AgentStart => {
				self.working = true;
				self.chat.scroll_to_bottom();
			}
			AgentEvent::AgentEnd { .. } => {
				self.working = false;
				self.refresh_branch();
				self.chat.scroll_to_bottom();
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
				self.chat.scroll_to_bottom();
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
				self.chat.scroll_to_bottom();
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
				self.chat.scroll_to_bottom();
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
		self.chat.scroll_to_bottom();
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
		// Footer grows to three lines while an extension status item is set
		// (footer.ts:289-299 appends the status line when non-empty).
		let footer_rows = 2 + u16::from(self.extension_status.is_some());
		let chunks = Layout::vertical([
			Constraint::Length(1),
			Constraint::Min(3),
			Constraint::Length(editor_rows + 2),
			Constraint::Length(footer_rows),
		])
		.split(area);

		self.draw_header(frame, chunks[0]);
		let chat_width = chunks[1].width.saturating_sub(2) as usize;
		let lines = self.render_blocks(chat_width);
		self.chat.set_lines(lines);
		self.chat.render(chunks[1], frame.buffer_mut());
		self.draw_editor(frame, chunks[2]);
		self.draw_footer(frame, chunks[3]);
		self.draw_slash_menu(frame, chunks[1], chunks[2]);
		self.draw_overlay(frame, chunks[1]);
		self.draw_settings_overlay(frame, chunks[1]);
		self.draw_scoped_overlay(frame, chunks[1]);
		self.draw_tree(frame, chunks[1]);
		self.draw_dialog(frame, chunks[1]);
		self.draw_extension_dialog(frame, chunks[1]);
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
		self.draw_session_overlay(frame, chunks[1]);
		self.draw_trust_overlay(frame, chunks[1]);
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
		let model = self.session.live_agent().model();
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
		let live = self.session.live_agent();
		let usage = crate::core::context_usage::compute(&live);
		let model_window = live.model().context_window;
		let auto_compaction = self.session.auto_compaction.load(Ordering::Relaxed);
		let (context_text, context_style) =
			footer_context_segment(usage, model_window, auto_compaction, &self.theme);

		let model = self.session.live_agent().model();
		let thinking = self.session.live_agent().thinking_level();
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

		// Line 3: extension status items (footer.ts:289-299). The host shim
		// drops the upstream `key`, so this is the single slot's text,
		// sanitized the same way (newlines/tabs → spaces, collapsed).
		if let Some(status) = self.extension_status.as_ref() {
			let status = sanitize_status_text(status);
			if !status.is_empty() {
				let status_area = Rect {
					y: area.y + 2,
					height: 1,
					..area
				};
				frame.render_widget(
					Paragraph::new(Line::from(Span::styled(
						truncate_to_width(&status, width),
						Style::new().fg(self.theme.muted),
					))),
					status_area,
				);
			}
		}
	}

	/// Model selector overlay (model-selector.ts): bordered panel with a search
	/// Slash-command menu floating directly above the editor, editor-width
	/// and bottom-anchored in the chat area (upstream renders the
	/// autocomplete list just above the input).
	fn draw_slash_menu(&mut self, frame: &mut Frame, chat_area: Rect, editor_area: Rect) {
		let Some(menu) = self.slash_menu.as_ref() else {
			return;
		};
		let visible = menu.list.filtered().len().clamp(1, 8) as u16;
		let height = (visible + 2).min(chat_area.height);
		let area = Rect {
			x: editor_area.x,
			y: chat_area.y + chat_area.height.saturating_sub(height),
			width: editor_area.width,
			height,
		};
		frame.render_widget(Clear, area);
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.border))
			.title(Span::styled(
				" Commands ",
				Style::new().fg(self.theme.accent),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);
		let lines = menu.list.render_lines(inner.width as usize, &self.theme);
		frame.render_widget(Paragraph::new(lines), inner);
	}

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
			Dialog::SettingInput { key, value } => (
				" Setting value ",
				format!("{key}\n\n{value}▌\n\nEnter save · Esc back to settings"),
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

	/// Extension dialog (`ui.select`/`ui.confirm`/`ui.input`) — the same
	/// centered box as the core dialogs, with the extension's title.
	fn draw_extension_dialog(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(state) = self.extension_dialog.as_ref() else {
			return;
		};
		let (title, body) = match &state.request.kind {
			crate::core::extensions::ExtensionDialogKind::Select { title, options } => {
				let listed = options
					.iter()
					.enumerate()
					.map(|(index, option)| {
						let marker = if index == state.cursor { "❯ " } else { "  " };
						format!("{marker}{option}")
					})
					.collect::<Vec<_>>()
					.join("\n");
				(
					format!(" {title} "),
					format!(
						"{listed}\n\n{} select · {} cancel",
						self.keybinding_hint("tui.select.confirm", "enter"),
						self.keybinding_hint("tui.select.cancel", "esc"),
					),
				)
			}
			crate::core::extensions::ExtensionDialogKind::Confirm { title } => (
				format!(" {title} "),
				"Y/Enter confirm · N/Esc cancel".to_string(),
			),
			crate::core::extensions::ExtensionDialogKind::Input { title, placeholder } => {
				let (value, cursor) = if state.value.is_empty() {
					(
						placeholder.clone().unwrap_or_default(),
						if placeholder.is_some() { "" } else { "▌" },
					)
				} else {
					(state.value.clone(), "▌")
				};
				(
					format!(" {title} "),
					format!(
						"{value}{cursor}\n\n{} submit · {} cancel",
						self.keybinding_hint("tui.input.submit", "enter"),
						self.keybinding_hint("tui.select.cancel", "esc"),
					),
				)
			}
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

	fn draw_trust_overlay(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some((list, info)) = self.trust_overlay.as_ref() else {
			return;
		};
		let width = chat_area.width.clamp(56, 84).min(chat_area.width);
		let height = (info.header.len() as u16 + list.filtered().len() as u16 + 3)
			.clamp(8, 16)
			.min(chat_area.height);
		let area = Rect {
			x: chat_area.x + (chat_area.width - width) / 2,
			y: chat_area.y + (chat_area.height - height) * 2 / 5,
			width,
			height,
		};
		frame.render_widget(Clear, area);
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.accent))
			.title(Span::styled(
				" Project trust ",
				Style::new()
					.fg(self.theme.accent)
					.add_modifier(Modifier::BOLD),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);
		// Header lines (cwd / saved decision / current session) in the muted
		// info style, then the option list.
		let mut lines: Vec<Line> = info
			.header
			.iter()
			.map(|line| {
				Line::from(Span::styled(
					line.clone(),
					Style::new().fg(self.theme.muted),
				))
			})
			.collect();
		lines.push(Line::default());
		let list_limit = (inner.height as usize).saturating_sub(lines.len()).max(1);
		lines.extend(list.render_lines_with_limit(inner.width as usize, &self.theme, list_limit));
		lines.resize(inner.height as usize, Line::default());
		frame.render_widget(Paragraph::new(lines), inner);
	}

	/// `/settings` selector — the settings list in the model-overlay panel
	/// shape (the upstream SettingsList panel).
	fn draw_settings_overlay(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(overlay) = self.settings_overlay.as_ref() else {
			return;
		};
		let width = chat_area.width.clamp(48, 72).min(chat_area.width);
		let height = chat_area.height.min(18);
		let area = Rect {
			x: chat_area.x + (chat_area.width - width) / 2,
			y: chat_area.y + (chat_area.height - height) * 2 / 5,
			width,
			height,
		};
		frame.render_widget(Clear, area);
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.accent))
			.title(Span::styled(
				" Settings ",
				Style::new()
					.fg(self.theme.accent)
					.add_modifier(Modifier::BOLD),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);
		let mut lines = overlay.list.render_lines(inner.width as usize, &self.theme);
		lines.truncate((inner.height as usize).saturating_sub(1));
		lines.push(Line::from(Span::styled(
			format!(
				" {} cycle/edit · {} cancel",
				self.keybinding_hint("tui.select.confirm", "enter"),
				self.keybinding_hint("tui.select.cancel", "esc"),
			),
			Style::new().fg(self.theme.muted),
		)));
		lines.resize(inner.height as usize, Line::default());
		frame.render_widget(Paragraph::new(lines), inner);
	}

	/// `/scoped-models` multi-select with the search line and the toggle /
	/// apply hint line (upstream ScopedModelsSelector footer).
	fn draw_scoped_overlay(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(overlay) = self.scoped_overlay.as_ref() else {
			return;
		};
		let width = chat_area.width.clamp(48, 76).min(chat_area.width);
		let height = chat_area.height.min(18);
		let area = Rect {
			x: chat_area.x + (chat_area.width - width) / 2,
			y: chat_area.y + (chat_area.height - height) * 2 / 5,
			width,
			height,
		};
		frame.render_widget(Clear, area);
		let block = Block::bordered()
			.border_style(Style::new().fg(self.theme.accent))
			.title(Span::styled(
				" Scoped models ",
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
		let mut lines = vec![search_line];
		let list_limit = (inner.height as usize).saturating_sub(3).max(1);
		let list_lines =
			overlay
				.list
				.render_lines_with_limit(inner.width as usize, &self.theme, list_limit);
		lines.extend(list_lines);
		lines.push(Line::from(Span::styled(
			format!(
				" {} toggle · tab apply · {} cancel · ✓ = current model · type to filter",
				self.keybinding_hint("tui.select.confirm", "enter"),
				self.keybinding_hint("tui.select.cancel", "esc"),
			),
			Style::new().fg(self.theme.muted),
		)));
		lines.resize(inner.height as usize, Line::default());
		frame.render_widget(Paragraph::new(lines), inner);
	}

	fn draw_session_overlay(&mut self, frame: &mut Frame, chat_area: Rect) {
		let Some(overlay) = self.session_overlay.as_mut() else {
			return;
		};
		let width = chat_area.width.clamp(60, 100).min(chat_area.width);
		let height = chat_area.height.saturating_sub(4).max(10);
		if chat_area.width < width || chat_area.height < height {
			return;
		}
		let x = chat_area.x + (chat_area.width - width) / 2;
		let y = chat_area.y + (chat_area.height - height) / 2;
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
				" Resume session ",
				Style::new()
					.fg(self.theme.accent)
					.add_modifier(Modifier::BOLD),
			));
		let inner = block.inner(area);
		frame.render_widget(block, area);
		let lines = overlay.render(
			&self.theme,
			&self.keybindings,
			inner.width as usize,
			inner.height as usize,
		);
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
				ChatBlock::Bash {
					command,
					output,
					status,
					rendered,
				} => {
					if let Some((w, cached)) = rendered
						&& *w == width
					{
						lines.extend(cached.iter().cloned());
						continue;
					}
					let out = render_bash_block(command, output, status, width, &theme);
					*rendered = Some((width, out.clone()));
					lines.extend(out);
					lines.push(Line::default());
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
				ChatBlock::Doc {
					title,
					text,
					rendered,
				} => {
					if let Some((w, cached)) = rendered
						&& *w == width
					{
						lines.extend(cached.iter().cloned());
						continue;
					}
					// Upstream: DynamicBorder, bold accent title, Markdown,
					// DynamicBorder.
					let border = "─".repeat(width.max(3));
					let mut out = vec![Line::from(Span::styled(
						border.clone(),
						Style::new().fg(theme.border),
					))];
					out.push(Line::from(Span::styled(
						title.clone(),
						Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
					)));
					out.push(Line::default());
					out.extend(markdown_lines(text, width.saturating_sub(2).max(1), &theme));
					out.push(Line::from(Span::styled(
						border,
						Style::new().fg(theme.border),
					)));
					out.push(Line::default());
					*rendered = Some((width, out.clone()));
					lines.extend(out);
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

/// footer.ts:14-20 `sanitizeStatusText`: newlines/tabs/carriage returns
/// become spaces, runs of spaces collapse, edges trim.
fn sanitize_status_text(text: &str) -> String {
	let flattened: String = text
		.chars()
		.map(|c| {
			if c == '\n' || c == '\r' || c == '\t' {
				' '
			} else {
				c
			}
		})
		.collect();
	flattened.split_whitespace().collect::<Vec<_>>().join(" ")
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

// -- /hotkeys markdown (upstream handleHotkeysCommand) ------------------------

/// Capitalized chord display (upstream `keyDisplayText`/`formatKeys` with
/// `capitalize: true`): every `+`-separated segment gets an uppercase first
/// letter, inner casing preserved (`pageUp` → `PageUp`).
fn key_display_text(chord: &str) -> String {
	chord
		.split('+')
		.map(|part| match part.chars().next() {
			Some(first) => first.to_uppercase().collect::<String>() + &part[first.len_utf8()..],
			None => String::new(),
		})
		.collect::<Vec<_>>()
		.join("+")
}

/// The resolved keybindings as the upstream markdown table (Navigation /
/// Editing / Other sections; the same rows and action names as
/// `handleHotkeysCommand`). Multi-chord actions join their chords with `/`,
/// like the upstream cells. Unbound actions render as `unbound` (upstream's
/// `keyDisplayText` yields the empty string there). The extension-shortcut
/// section has no port counterpart.
fn hotkeys_markdown(keybindings: &KeybindingsManager) -> String {
	let action_cell = |actions: &[&str]| -> String {
		actions
			.iter()
			.map(|action| {
				let keys = keybindings.formatted_keys(action).unwrap_or_default();
				if keys.is_empty() {
					return "unbound".to_string();
				}
				keys.iter()
					.map(|chord| key_display_text(chord))
					.collect::<Vec<_>>()
					.join("/")
			})
			.map(|keys| format!("`{keys}`"))
			.collect::<Vec<_>>()
			.join(" / ")
	};
	let mut table = String::new();
	let mut section = |title: &str, rows: &[(&str, String)]| {
		table.push_str(&format!(
			"\n**{title}**\n\n| Key | Action |\n|-----|--------|\n"
		));
		for (label, keys) in rows {
			table.push_str(&format!("| {keys} | {label} |\n"));
		}
	};
	let mut rows: Vec<(&str, String)> = vec![
		(
			"Move cursor / browse history",
			action_cell(&[
				"tui.editor.cursorUp",
				"tui.editor.cursorDown",
				"tui.editor.cursorLeft",
				"tui.editor.cursorRight",
			]),
		),
		(
			"Move by word",
			action_cell(&["tui.editor.cursorWordLeft", "tui.editor.cursorWordRight"]),
		),
		(
			"Start of line",
			action_cell(&["tui.editor.cursorLineStart"]),
		),
		("End of line", action_cell(&["tui.editor.cursorLineEnd"])),
		(
			"Jump forward to character",
			action_cell(&["tui.editor.jumpForward"]),
		),
		(
			"Jump backward to character",
			action_cell(&["tui.editor.jumpBackward"]),
		),
		(
			"Scroll by page",
			action_cell(&["tui.editor.pageUp", "tui.editor.pageDown"]),
		),
	];
	section("Navigation", &rows);
	rows = vec![
		("Send message", action_cell(&["tui.input.submit"])),
		("New line", action_cell(&["tui.input.newLine"])),
		(
			"Delete word backwards",
			action_cell(&["tui.editor.deleteWordBackward"]),
		),
		(
			"Delete word forwards",
			action_cell(&["tui.editor.deleteWordForward"]),
		),
		(
			"Delete to start of line",
			action_cell(&["tui.editor.deleteToLineStart"]),
		),
		(
			"Delete to end of line",
			action_cell(&["tui.editor.deleteToLineEnd"]),
		),
		(
			"Paste the most-recently-deleted text",
			action_cell(&["tui.editor.yank"]),
		),
		(
			"Cycle through the deleted text after pasting",
			action_cell(&["tui.editor.yankPop"]),
		),
		("Undo", action_cell(&["tui.editor.undo"])),
	];
	section("Editing", &rows);
	// The literal prefix rows (upstream hardcodes `/`, `!`, `!!` in its
	// table) close the section.
	rows = vec![
		(
			"Path completion / accept autocomplete",
			action_cell(&["tui.input.tab"]),
		),
		(
			"Cancel autocomplete / abort streaming",
			action_cell(&["app.interrupt"]),
		),
		(
			"Clear editor (first) / exit (second)",
			action_cell(&["app.clear"]),
		),
		("Exit (when editor is empty)", action_cell(&["app.exit"])),
		("Suspend to background", action_cell(&["app.suspend"])),
		("Cycle thinking level", action_cell(&["app.thinking.cycle"])),
		(
			"Cycle models",
			action_cell(&["app.model.cycleForward", "app.model.cycleBackward"]),
		),
		("Open model selector", action_cell(&["app.model.select"])),
		(
			"Toggle tool output expansion",
			action_cell(&["app.tools.expand"]),
		),
		(
			"Toggle thinking block visibility",
			action_cell(&["app.thinking.toggle"]),
		),
		(
			"Edit message in external editor",
			action_cell(&["app.editor.external"]),
		),
		(
			"Copy selection or last assistant message",
			action_cell(&["app.message.copy"]),
		),
		(
			"Queue follow-up message",
			action_cell(&["app.message.followUp"]),
		),
		(
			"Restore queued messages",
			action_cell(&["app.message.dequeue"]),
		),
		(
			"Paste files on macOS, images, or text from clipboard",
			action_cell(&["app.clipboard.pasteImage"]),
		),
		("Slash commands", "`/`".to_string()),
		("Run bash command", "`!`".to_string()),
		(
			"Run bash command (excluded from context)",
			"`!!`".to_string(),
		),
	];
	section("Other", &rows);
	table.trim().to_string()
}

// -- /settings entries (upstream SettingsSelectorComponent, pi-rs cut) --------

/// The selector's entries: every settings key pi-rs persists, in the
/// upstream settings-menu spirit. `instant` entries hot-apply
/// (thinking level, theme, tree filter mode).
fn settings_entries() -> Vec<SettingEntry> {
	let cycle = |values: &[&str]| {
		SettingEditor::Cycle(values.iter().map(|value| (*value).to_string()).collect())
	};
	vec![
		SettingEntry {
			key: "defaultProvider",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "defaultModel",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "defaultThinkingLevel",
			editor: cycle(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
			instant: true,
		},
		SettingEntry {
			key: "theme",
			editor: SettingEditor::Cycle(available_theme_names()),
			instant: true,
		},
		SettingEntry {
			key: "externalEditor",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "shellPath",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "compaction.enabled",
			editor: cycle(&["true", "false"]),
			instant: false,
		},
		SettingEntry {
			key: "compaction.reserveTokens",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "compaction.keepRecentTokens",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "retry.enabled",
			editor: cycle(&["true", "false"]),
			instant: false,
		},
		SettingEntry {
			key: "retry.maxRetries",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "retry.baseDelayMs",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "retry.maxAgentDelayMs",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "branchSummary.skipPrompt",
			editor: cycle(&["true", "false"]),
			instant: false,
		},
		SettingEntry {
			key: "treeFilterMode",
			editor: cycle(&["default", "no-tools", "user-only", "labeled-only", "all"]),
			instant: true,
		},
		SettingEntry {
			key: "sessionDir",
			editor: SettingEditor::Input,
			instant: false,
		},
		SettingEntry {
			key: "defaultTools",
			editor: SettingEditor::Input,
			instant: false,
		},
	]
}

/// Theme names available for the settings cycle: the built-ins plus the
/// user themes directory (`getAvailableThemes` upstream).
fn available_theme_names() -> Vec<String> {
	let mut names = vec!["dark".to_string(), "light".to_string()];
	let Ok(entries) = std::fs::read_dir(crate::config::themes_dir()) else {
		return names;
	};
	let mut customs: Vec<String> = entries
		.filter_map(|entry| entry.ok())
		.filter_map(|entry| {
			let path = entry.path();
			(path.extension().and_then(|ext| ext.to_str()) == Some("json")).then(|| {
				path.file_stem()
					.map(|stem| stem.to_string_lossy().into_owned())
			})
		})
		.flatten()
		.filter(|name| name != "dark" && name != "light")
		.collect();
	customs.sort();
	names.extend(customs);
	names
}

/// The current display value of a settings key (upstream feeds the component
/// from its getters).
fn setting_value(effective: &crate::core::settings_manager::Settings, key: &str) -> String {
	use crate::core::settings_manager::normalize_tree_filter_mode;
	let text_or = |value: &Option<String>, fallback: &str| {
		value
			.clone()
			.filter(|value| !value.trim().is_empty())
			.unwrap_or_else(|| fallback.to_string())
	};
	let bool_or = |value: Option<bool>| match value {
		Some(value) => value.to_string(),
		None => "not set".to_string(),
	};
	let number_or = |value: Option<u64>| match value {
		Some(value) => value.to_string(),
		None => "not set".to_string(),
	};
	match key {
		"defaultProvider" => text_or(&effective.default_provider, "not set"),
		"defaultModel" => text_or(&effective.default_model, "not set"),
		"defaultThinkingLevel" => text_or(&effective.default_thinking_level, "not set"),
		"theme" => text_or(&effective.theme, "default"),
		"externalEditor" => text_or(&effective.external_editor, "not set"),
		"shellPath" => text_or(&effective.shell_path, "not set"),
		"sessionDir" => text_or(&effective.session_dir, "not set"),
		"defaultTools" => match &effective.default_tools {
			Some(tools) if !tools.is_empty() => tools.join(" "),
			_ => "not set".to_string(),
		},
		"treeFilterMode" => {
			normalize_tree_filter_mode(effective.tree_filter_mode.as_deref()).to_string()
		}
		"compaction.enabled" => bool_or(effective.compaction.as_ref().and_then(|c| c.enabled)),
		"compaction.reserveTokens" => {
			number_or(effective.compaction.as_ref().and_then(|c| c.reserve_tokens))
		}
		"compaction.keepRecentTokens" => number_or(
			effective
				.compaction
				.as_ref()
				.and_then(|c| c.keep_recent_tokens),
		),
		"retry.enabled" => bool_or(effective.retry.as_ref().and_then(|r| r.enabled)),
		"retry.maxRetries" => number_or(
			effective
				.retry
				.as_ref()
				.and_then(|r| r.max_retries)
				.map(u64::from),
		),
		"retry.baseDelayMs" => number_or(effective.retry.as_ref().and_then(|r| r.base_delay_ms)),
		"retry.maxAgentDelayMs" => {
			number_or(effective.retry.as_ref().and_then(|r| r.max_agent_delay_ms))
		}
		"branchSummary.skipPrompt" => bool_or(
			effective
				.branch_summary
				.as_ref()
				.and_then(|b| b.skip_prompt),
		),
		_ => "not set".to_string(),
	}
}

/// Parse one settings override (the validated edit): typed fields reject
/// bad values with the same message shape the editor shows. `Settings`
/// carries only the overridden field so [`SettingsManager::set_overrides`]
/// merges the rest.
fn setting_override(
	key: &str,
	raw: &str,
) -> Result<crate::core::settings_manager::Settings, String> {
	use crate::core::settings_manager::{
		BranchSummarySettings, CompactionSettings, RetrySettings, Settings,
		normalize_tree_filter_mode,
	};
	let text = |raw: &str| -> Result<String, String> {
		let trimmed = raw.trim();
		if trimmed.is_empty() {
			// Clearing is not reachable through the port's overrides writer;
			// the field must be removed from settings.json by hand.
			return Err("A value is required (empty values are not saved; remove the key from settings.json to clear it)".into());
		}
		Ok(trimmed.to_string())
	};
	let number = |raw: &str| -> Result<u64, String> {
		raw.trim()
			.parse::<u64>()
			.map_err(|_| format!("{key} expects a number (got {raw:?})"))
	};
	let number_u32 = |raw: &str| -> Result<u32, String> {
		raw.trim()
			.parse::<u32>()
			.map_err(|_| format!("{key} expects a number (got {raw:?})"))
	};
	let boolean = |raw: &str| -> Result<bool, String> {
		match raw.trim() {
			"true" => Ok(true),
			"false" => Ok(false),
			other => Err(format!("{key} expects true or false (got {other:?})")),
		}
	};
	let settings = match key {
		"defaultProvider" => Settings {
			default_provider: Some(text(raw)?),
			..Settings::default()
		},
		"defaultModel" => Settings {
			default_model: Some(text(raw)?),
			..Settings::default()
		},
		"defaultThinkingLevel" => {
			let value = text(raw)?;
			if parse_setting_thinking_level(&value).is_none() {
				return Err(format!(
					"{key} expects off/minimal/low/medium/high/xhigh/max (got {value:?})"
				));
			}
			Settings {
				default_thinking_level: Some(value),
				..Settings::default()
			}
		}
		"theme" => Settings {
			theme: Some(text(raw)?),
			..Settings::default()
		},
		"externalEditor" => Settings {
			external_editor: Some(text(raw)?),
			..Settings::default()
		},
		"shellPath" => Settings {
			shell_path: Some(text(raw)?),
			..Settings::default()
		},
		"sessionDir" => Settings {
			session_dir: Some(text(raw)?),
			..Settings::default()
		},
		"defaultTools" => Settings {
			default_tools: Some(
				text(raw)?
					.split_whitespace()
					.map(str::to_string)
					.collect::<Vec<_>>(),
			),
			..Settings::default()
		},
		"treeFilterMode" => {
			let value = text(raw)?;
			if normalize_tree_filter_mode(Some(value.as_str())) != value {
				return Err(format!(
					"{key} expects default/no-tools/user-only/labeled-only/all (got {value:?})"
				));
			}
			Settings {
				tree_filter_mode: Some(value),
				..Settings::default()
			}
		}
		"compaction.enabled" => Settings {
			compaction: Some(CompactionSettings {
				enabled: Some(boolean(raw)?),
				..CompactionSettings::default()
			}),
			..Settings::default()
		},
		"compaction.reserveTokens" => Settings {
			compaction: Some(CompactionSettings {
				reserve_tokens: Some(number(raw)?),
				..CompactionSettings::default()
			}),
			..Settings::default()
		},
		"compaction.keepRecentTokens" => Settings {
			compaction: Some(CompactionSettings {
				keep_recent_tokens: Some(number(raw)?),
				..CompactionSettings::default()
			}),
			..Settings::default()
		},
		"retry.enabled" => Settings {
			retry: Some(RetrySettings {
				enabled: Some(boolean(raw)?),
				..RetrySettings::default()
			}),
			..Settings::default()
		},
		"retry.maxRetries" => Settings {
			retry: Some(RetrySettings {
				max_retries: Some(number_u32(raw)?),
				..RetrySettings::default()
			}),
			..Settings::default()
		},
		"retry.baseDelayMs" => Settings {
			retry: Some(RetrySettings {
				base_delay_ms: Some(number(raw)?),
				..RetrySettings::default()
			}),
			..Settings::default()
		},
		"retry.maxAgentDelayMs" => Settings {
			retry: Some(RetrySettings {
				max_agent_delay_ms: Some(number(raw)?),
				..RetrySettings::default()
			}),
			..Settings::default()
		},
		"branchSummary.skipPrompt" => Settings {
			branch_summary: Some(BranchSummarySettings {
				skip_prompt: Some(boolean(raw)?),
			}),
			..Settings::default()
		},
		other => return Err(format!("Unknown setting {other:?}")),
	};
	Ok(settings)
}

/// Setting-string → [`ThinkingLevel`] for the hot-applied default.
fn parse_setting_thinking_level(raw: &str) -> Option<ThinkingLevel> {
	match raw.trim().to_lowercase().as_str() {
		"off" => Some(ThinkingLevel::Off),
		"minimal" => Some(ThinkingLevel::Minimal),
		"low" => Some(ThinkingLevel::Low),
		"medium" => Some(ThinkingLevel::Medium),
		"high" => Some(ThinkingLevel::High),
		"xhigh" => Some(ThinkingLevel::Xhigh),
		"max" => Some(ThinkingLevel::Max),
		_ => None,
	}
}

// -- /scoped-models helpers (upstream ScopedModelsSelectorComponent) ----------

/// The selector's rows: `[x]`/`[ ]` checkmark prefix (the upstream
/// component's checkbox), the `provider/id` reference as the value, and the
/// context-window description line of the model selector.
fn scoped_model_items(
	all: &[Model],
	checked: &std::collections::BTreeSet<String>,
	default_key: &str,
) -> Vec<SelectItem> {
	all.iter()
		.map(|model| {
			let key = model.key();
			let checkbox = if checked.contains(&key) { "[x]" } else { "[ ]" };
			let mut description = format!("[{}]", model.provider);
			if model.context_window > 0 {
				description.push_str(&format!(" · {} ctx", format_tokens(model.context_window)));
			}
			if model.key() == default_key {
				description.push_str(" · default");
			}
			SelectItem {
				value: key,
				label: format!("{checkbox} {}", model.id.clone()),
				description: Some(description),
			}
		})
		.collect()
}

/// The checked keys resolved against the catalog in catalog order — the
/// `Vec<Model>` handed to [`crate::core::model_registry::ModelRegistry::set_model_scope`].
fn scope_models_for(all: &[Model], checked: &std::collections::BTreeSet<String>) -> Vec<Model> {
	all.iter()
		.filter(|model| checked.contains(&model.key()))
		.cloned()
		.collect()
}

/// Quote path with single quotes if it contains spaces or shell metacharacters.
fn quote_if_needed(s: &str) -> String {
	if s.chars()
		.any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | '`'))
	{
		format!("'{}'", s.replace('\'', "'\\''"))
	} else {
		s.to_string()
	}
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
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
			crate::core::session_manager::SessionManager::create_with_parent_in(&root, &root, None)
				.unwrap();
		saved
			.append_message(&Message::user("saved question"))
			.unwrap();
		let path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let agent = pi_agent::Agent::new(pi_agent::AgentOptions::default());
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
		let mut named = crate::core::session_manager::SessionManager::create_with_parent_in(
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
		let mut unnamed = crate::core::session_manager::SessionManager::create_with_parent_in(
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
		let saved =
			crate::core::session_manager::SessionManager::create_with_parent_in(&root, &root, None)
				.unwrap();
		let path = saved.session_dir().unwrap().to_path_buf();
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::session_manager::SessionManager::in_memory(),
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
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
		);
		let ui = UiConfig {
			theme: Theme::dark(),
			keybindings: crate::core::ui_config::new_keybindings_manager(),
			notices: Vec::new(),
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
		// `/session` renders the upstream "Session Info" panel: the session
		// name and the (in-memory) log location.
		assert!(mode.blocks.iter().any(|block| matches!(block, ChatBlock::Notice(text) if text.contains("test session") && text.contains("In-memory"))));
		mode.session.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[test]
	fn slash_menu_query_tracks_the_single_token_rule() {
		// Upstream getSuggestions: only a leading `/` token before any space
		// opens the command menu.
		assert_eq!(slash_command_query("/"), Some(""));
		assert_eq!(slash_command_query("/mo"), Some("mo"));
		assert_eq!(slash_command_query("/model"), Some("model"));
		assert_eq!(
			slash_command_query("/model x"),
			None,
			"argument position closes the menu"
		);
		assert_eq!(slash_command_query("model"), None, "no slash, no menu");
		assert_eq!(
			slash_command_query("say /mo"),
			None,
			"slash must lead the text"
		);
	}

	#[test]
	fn slash_menu_completion_adds_the_argument_space() {
		assert_eq!(apply_slash_completion("model"), "/model ");
	}

	// -- /hotkeys, /settings, /scoped-models helpers ------------------------------

	#[test]
	fn hotkeys_markdown_lists_the_upstream_sections_with_resolved_keys() {
		let keybindings = crate::core::ui_config::new_keybindings_manager();
		let markdown = hotkeys_markdown(&keybindings);
		for section in ["**Navigation**", "**Editing**", "**Other**"] {
			assert!(markdown.contains(section), "missing {section}: {markdown}");
		}
		// Resolved chords are capitalized (keyDisplayText style).
		assert!(markdown.contains("`Ctrl+O`"), "{}", markdown);
		assert!(markdown.contains("`Escape`"), "{}", markdown);
		// The upstream row set: sample rows from each section.
		assert!(markdown.contains("Move cursor / browse history"));
		assert!(markdown.contains("Send message"));
		assert!(markdown.contains("Copy selection or last assistant message"));
		// The literal prefix rows survive.
		assert!(markdown.contains("| `/` | Slash commands |"));
		assert!(markdown.contains("| `!` | Run bash command |"));
		assert!(markdown.contains("| `!!` | Run bash command (excluded from context) |"));
	}

	#[test]
	fn key_display_text_capitalizes_segments_and_keeps_inner_casing() {
		assert_eq!(key_display_text("ctrl+o"), "Ctrl+O");
		assert_eq!(key_display_text("shift+ctrl+p"), "Shift+Ctrl+P");
		assert_eq!(key_display_text("pageup"), "Pageup");
		assert_eq!(key_display_text("alt+left"), "Alt+Left");
	}

	#[test]
	fn setting_override_validates_typed_fields_and_produces_partial_settings() {
		let overrides = setting_override("compaction.reserveTokens", " 4096 ").unwrap();
		assert_eq!(
			overrides.compaction.as_ref().unwrap().reserve_tokens,
			Some(4096)
		);
		assert!(
			overrides.theme.is_none(),
			"only the overridden field is set"
		);
		let retry = setting_override("retry.maxRetries", "5").unwrap();
		assert_eq!(retry.retry.as_ref().unwrap().max_retries, Some(5));
		assert!(setting_override("retry.maxRetries", "many").is_err());
		assert!(setting_override("retry.enabled", "yes").is_err());
		assert!(setting_override("treeFilterMode", "everything").is_err());
		assert!(setting_override("defaultThinkingLevel", "loud").is_err());
		assert!(setting_override("theme", "").is_err());
		// Whitespace-separated defaultTools split into the list.
		let tools = setting_override("defaultTools", "read bash").unwrap();
		assert_eq!(
			tools.default_tools,
			Some(vec!["read".into(), "bash".into()])
		);
		// Unknown keys are rejected, not silently written.
		assert!(setting_override("showImages", "true").is_err());
	}

	#[test]
	fn setting_value_reports_effective_defaults() {
		use crate::core::settings_manager::{Settings, normalize_tree_filter_mode};
		let settings = Settings::default();
		assert_eq!(setting_value(&settings, "defaultProvider"), "not set");
		assert_eq!(setting_value(&settings, "theme"), "default");
		assert_eq!(setting_value(&settings, "treeFilterMode"), "default");
		assert_eq!(setting_value(&settings, "retry.enabled"), "not set");
		let configured: Settings = serde_json::from_str(
			r#"{"defaultProvider":"acme","defaultTools":["read","bash"],"compaction":{"enabled":false}}"#,
		)
		.unwrap();
		assert_eq!(setting_value(&configured, "defaultProvider"), "acme");
		assert_eq!(setting_value(&configured, "defaultTools"), "read bash");
		assert_eq!(setting_value(&configured, "compaction.enabled"), "false");
		// normalize_tree_filter_mode participates through setting_value.
		assert_eq!(normalize_tree_filter_mode(Some("all")), "all");
	}

	#[test]
	fn scoped_model_items_check_and_uncheck_and_scope_models_for_resolves_catalog_order() {
		let registry = crate::core::model_registry::ModelRegistry::load(None);
		let all = registry.all_models().to_vec();
		assert!(all.len() >= 2, "the builtin catalog seeds the selector");
		let default_key = registry.default_model().key();
		// Checked rows show [x], the default model line carries the marker.
		let checked: std::collections::BTreeSet<String> =
			all.iter().take(1).map(|model| model.key()).collect();
		let items = scoped_model_items(&all, &checked, &default_key);
		assert_eq!(items.len(), all.len());
		assert!(items[0].label.starts_with("[x] "), "{}", items[0].label);
		assert!(items[1].label.starts_with("[ ] "), "{}", items[1].label);
		assert!(
			items.iter().any(|item| item
				.description
				.as_deref()
				.is_some_and(|d| d.contains("default"))),
			"the default model line is marked"
		);
		// The scope resolves in catalog order; an empty set resolves empty.
		let scoped = scope_models_for(&all, &checked);
		assert_eq!(scoped.len(), 1);
		assert_eq!(scoped[0].key(), all[0].key());
		assert!(scope_models_for(&all, &std::collections::BTreeSet::new()).is_empty());
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
			external_editor: crate::core::ui_config::resolve_external_editor(None),
			initial_images: Vec::new(),
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
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::create_with_parent_in(&root, &root, None)
				.unwrap();
		saved
			.append_message(&Message::user("clonable question"))
			.unwrap();
		let saved_path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::create_with_parent_in(&root, &root, None)
				.unwrap();
		saved
			.append_message(&Message::user("forkable question"))
			.unwrap();
		let saved_path = saved.session_dir().unwrap().to_path_buf();
		drop(saved);
		let handle = crate::core::agent_session::spawn_agent_session(
			pi_agent::Agent::new(pi_agent::AgentOptions::default()),
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::in_memory(),
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
			crate::core::session_manager::SessionManager::create_with_parent_in(&root, &root, None)
				.unwrap();
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
			crate::core::session_manager::SessionManager::in_memory(),
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

#[cfg(test)]
mod session_selector_tests {
	use super::*;
	use pi_ai::types::{AssistantMessage, StopReason, TextContent, ToolResultMessage, UserMessage};

	fn assistant_with_call_and_text(call_id: &str, text: &str) -> AssistantMessage {
		AssistantMessage {
			content: vec![
				pi_ai::AssistantContent::Text(TextContent {
					text: text.into(),
					text_signature: None,
				}),
				pi_ai::AssistantContent::ToolCall(pi_ai::ToolCall {
					id: call_id.into(),
					name: "bash".into(),
					arguments: serde_json::json!({"command": "ls"}),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				}),
			],
			api: "openai-completions".into(),
			provider: "mimo".into(),
			model: "mimo-v2.6".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: pi_ai::Usage {
				input: 100,
				output: 10,
				..pi_ai::Usage::default()
			},
			stop_reason: StopReason::Stop,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		}
	}

	#[test]
	fn blocks_from_messages_pairs_tool_calls_with_results() {
		let messages = vec![
			Message::User(UserMessage::text("run ls")),
			Message::Assistant(assistant_with_call_and_text("call-1", "doing it")),
			Message::ToolResult(ToolResultMessage::text_result(
				"call-1", "bash", "file.txt", false,
			)),
		];
		let blocks = blocks_from_messages(&messages);
		assert_eq!(blocks.len(), 3, "user + assistant + tool");
		let ChatBlock::User(text) = &blocks[0] else {
			panic!("user block");
		};
		assert_eq!(text, "run ls");
		let ChatBlock::Tool {
			id,
			output,
			running,
			..
		} = &blocks[2]
		else {
			panic!("tool block");
		};
		assert_eq!(id, "call-1");
		assert_eq!(output, "file.txt");
		assert!(!running);
		// A call without a persisted result renders with empty output.
		let messages = vec![Message::Assistant(assistant_with_call_and_text(
			"call-2", "lost",
		))];
		let ChatBlock::Tool {
			output, is_error, ..
		} = &blocks_from_messages(&messages)[1]
		else {
			panic!("tool block");
		};
		assert_eq!(output, "");
		assert!(!is_error);
	}

	#[test]
	fn usage_of_messages_sums_assistant_usage() {
		let messages = vec![
			Message::User(UserMessage::text("hi")),
			Message::Assistant(assistant_with_call_and_text("c", "a")),
			Message::Assistant(assistant_with_call_and_text("d", "b")),
		];
		let usage = usage_of_messages(&messages);
		assert_eq!(usage.0, 200);
		assert_eq!(usage.1, 20);
	}

	#[test]
	fn delete_session_file_removes_and_reports() {
		let path = std::env::temp_dir().join(format!(
			"pi-rs-delete-{}-{}.jsonl",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::write(&path, "{}\n").unwrap();
		let outcome = delete_session_file(&path).expect("delete succeeds");
		assert!(
			outcome == "Session moved to trash" || outcome == "Session deleted",
			"{outcome}"
		);
		assert!(!path.exists());
		// Deleting an already-gone file reports success (upstream treats a
		// vanished file as trashed, `!existsSync` branch).
		assert!(delete_session_file(&path).is_ok());
	}

	#[test]
	fn resolve_global_action_maps_the_model_cycle_chords() {
		let kb = crate::core::ui_config::new_keybindings_manager();
		let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
		assert_eq!(
			resolve_global_action(&kb, &ctrl_p, true),
			Some(GlobalAction::ModelCycleForward)
		);
		// The backward chord is platform-dependent: upstream uses
		// `windowsKeybindings ? "alt+p" : "shift+ctrl+p"` (keybindings.ts:45).
		let backward = if cfg!(target_os = "windows") {
			KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT)
		} else {
			KeyEvent::new(
				KeyCode::Char('p'),
				KeyModifiers::SHIFT | KeyModifiers::CONTROL,
			)
		};
		assert_eq!(
			resolve_global_action(&kb, &backward, true),
			Some(GlobalAction::ModelCycleBackward)
		);
	}

	#[test]
	fn resolve_global_action_maps_paste_clipboard() {
		let kb = crate::core::ui_config::new_keybindings_manager();
		let paste_key = if cfg!(target_os = "windows") {
			KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)
		} else {
			KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)
		};
		assert_eq!(
			resolve_global_action(&kb, &paste_key, true),
			Some(GlobalAction::PasteClipboard)
		);
		assert_eq!(
			resolve_global_action(&kb, &paste_key, false),
			Some(GlobalAction::PasteClipboard)
		);
	}

	#[test]
	fn quote_if_needed_quotes_spaces_and_specials() {
		assert_eq!(quote_if_needed("foo.png"), "foo.png");
		assert_eq!(
			quote_if_needed("/path/to/my file.png"),
			"'/path/to/my file.png'"
		);
		assert_eq!(quote_if_needed("foo'bar"), "'foo'\\''bar'");
	}

	#[test]
	fn session_tree_and_fork_actions_dispatch_through_overrides() {
		let mut kb = crate::core::ui_config::new_keybindings_manager();
		// Both actions ship unbound (upstream opens selectors); keybindings.json
		// can bind them to the read-only /tree view and the numbered /fork.
		kb.set_user_bindings(
			[
				("app.session.tree".to_string(), vec!["super+t".to_string()]),
				("app.session.fork".to_string(), vec!["super+f".to_string()]),
			]
			.into_iter()
			.collect(),
		)
		.unwrap();
		let tree_key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::SUPER);
		let fork_key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::SUPER);
		assert_eq!(
			resolve_global_action(&kb, &tree_key, true),
			Some(GlobalAction::SessionTree)
		);
		assert_eq!(
			resolve_global_action(&kb, &fork_key, true),
			Some(GlobalAction::SessionFork)
		);
	}

	#[test]
	fn resolve_global_action_maps_the_session_resume_action() {
		let kb = crate::core::ui_config::new_keybindings_manager();
		// app.session.resume has no default chord (upstream: unbound); it is
		// reachable via keybindings.json, so verify dispatch through a
		// synthetic override instead.
		let mut kb = kb;
		kb.set_user_bindings(
			[(
				"app.session.resume".to_string(),
				vec!["ctrl+shift+r".to_string()],
			)]
			.into_iter()
			.collect(),
		)
		.unwrap();
		let key = KeyEvent::new(
			KeyCode::Char('r'),
			KeyModifiers::CONTROL | KeyModifiers::SHIFT,
		);
		assert_eq!(
			resolve_global_action(&kb, &key, true),
			Some(GlobalAction::SessionResume)
		);
	}
}

#[cfg(test)]
mod external_editor_tests {
	use super::*;

	#[test]
	fn strip_editor_text_removes_bom_and_one_trailing_newline() {
		assert_eq!(strip_editor_text("hello\n"), "hello");
		assert_eq!(
			strip_editor_text("hello\n\n"),
			"hello\n",
			"only one newline"
		);
		assert_eq!(strip_editor_text("\u{feff}bom\n"), "bom");
		assert_eq!(strip_editor_text("no-newline"), "no-newline");
		assert_eq!(strip_editor_text("multi\nline\n"), "multi\nline");
	}

	#[tokio::test]
	async fn external_editor_success_returns_edited_content() {
		let dir = std::env::temp_dir().join(format!(
			"pi-editor-test-{}-{}",
			std::process::id(),
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap()
				.subsec_nanos()
		));
		std::fs::create_dir_all(&dir).unwrap();
		let script = dir.join("append-editor.sh");
		std::fs::write(&script, "#!/bin/sh\nprintf 'edited\\n' >> \"$1\"\n").unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
		}
		let outcome = run_external_editor(script.to_str().unwrap(), "original\n").await;
		let _ = std::fs::remove_dir_all(&dir);
		match outcome {
			ExternalEditorOutcome::Complete(content) => assert_eq!(content, "original\nedited"),
			ExternalEditorOutcome::Failed => panic!("expected success"),
		}
	}

	#[tokio::test]
	async fn external_editor_nonzero_exit_and_bad_command_fail() {
		assert!(matches!(
			run_external_editor("false", "content").await,
			ExternalEditorOutcome::Failed
		));
		assert!(matches!(
			run_external_editor("definitely-not-a-real-editor-xyz", "content").await,
			ExternalEditorOutcome::Failed
		));
	}

	#[test]
	fn ctrl_g_resolves_the_external_editor_action() {
		let kb = crate::core::ui_config::new_keybindings_manager();
		let key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
		assert_eq!(
			resolve_global_action(&kb, &key, false),
			Some(GlobalAction::EditorExternal)
		);
	}

	#[test]
	fn external_editor_resolution_prefers_configured_value() {
		assert_eq!(
			crate::core::ui_config::resolve_external_editor(Some("  my-edit-or ")),
			"my-edit-or"
		);
		assert!(!crate::core::ui_config::resolve_external_editor(Some("   ")).is_empty());
	}
}
