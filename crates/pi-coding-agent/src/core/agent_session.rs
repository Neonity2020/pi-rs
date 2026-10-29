//! Agent session — port of
//! `pi/packages/coding-agent/src/core/agent-session.ts` (reduced to the
//! framework-stage surface) plus the session-replacement runtime of
//! `core/agent-session-runtime.ts`.
//!
//! `AgentSession` is the mode-agnostic core shared by interactive/print/rpc:
//! it owns the [`Agent`], forwards agent events to subscribers, persists
//! messages to the session log, and serializes UI commands into agent calls
//! (prompt/steer/follow-up/abort/model/thinking). Compaction is wired end to
//! end: manual `/compact` and the automatic triggers share
//! `core/compaction.rs` — threshold checks run when a prompt is accepted
//! while idle, after a run completes, and between the turns of a multi-turn
//! run (the `prepare_next_turn` hook), and a context-overflow or
//! recoverable-length tail is omitted (see [`omit_failed_attempt`]),
//! compacted, and retried exactly once.
//!
//! Mid-run compaction deviation (against upstream `prepareNextTurn`): the
//! upstream session layer rewrites `agent.state.messages` directly between
//! turns; pi-agent refuses transcript replacements while a run is active
//! (`Agent::replace_messages*` guards on the active run), so the hook
//! commits the compaction entry to the log and aborts the run at the loop
//! boundary — the session task then swaps the transcript (now legal) and
//! resumes the interrupted work with `continue_run`. The log stays
//! authoritative: the entry is written before the swap, so a crash or a
//! failed swap still reconciles through the session projection on resume.
//!
//! Session replacement (`new_session`/`switch_session`/`fork`, port of
//! `AgentSessionRuntimeHost` in `agent-session-runtime.ts:196-345`): the
//! background task tears the current run down (abort + settle + persist),
//! builds the replacement [`SessionManager`] + [`Agent`], and swaps its
//! internal state in place. RPC consumers read the live session through the
//! shared slots on [`SessionHandle`] (`live_agent`, `log`,
//! `current_session_file`, `session_name`). Deviations, all declared:
//! - the rebuilt tool table is the built-in set for the (possibly new) cwd,
//!   filtered to the startup tool names; MCP tools (`mcp__*`) are dropped —
//!   the MCP assembly lives in `main.rs` and does not re-run on replacement
//!   (upstream re-registers them at `session_start`);
//! - the rebuilt system prompt assumes context files enabled (the
//!   `--no-context-files` CLI flag is not visible to the session task);
//! - `switch_session` refuses a target whose stored cwd does not exist
//!   (upstream `assertSessionCwdExists`); extension `session_before_switch`
//!   cancellation does not exist here, so `cancelled` is always false.
//!
//! Auto-retry (`set_auto_retry`/`abort_retry`, port of `_prepareRetry` /
//! `_willRetryAfterAgentEnd` / `_isRetryableError` in `agent-session.ts`):
//! after a run ends in a retryable error the failed attempt is omitted from
//! the model context (the same durable tombstone the overflow recovery uses)
//! and `continue_run` repeats the request after an exponential backoff.
//! Context overflow never reaches the retry — the compaction recovery owns
//! it (upstream `_isRetryableError` excludes overflow), so the two paths are
//! mutually exclusive by classification and never stack.
//!
//! Bash passthrough (`execute_bash`, port of `executeBash` /
//! `recordBashResult` in `agent-session.ts:3899-4010`): commands run through
//! the bash tool's pi-env executor with the session's `PI_RS_*` environment,
//! stream deltas as [`WireEvent::BashExecutionUpdate`], and persist as the
//! upstream `bashExecution` message entry. Results enter the live model
//! context only when the transcript is idle and fully persisted (they are
//! appended to it and the persist cursor is rebased); while a run is active
//! they are persisted to the log alone and reconcile through the projection
//! on resume — upstream flushes queued bash messages after `agent_end`,
//! which pi-rs approximates by deferring the *context* contribution only.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use pi_agent::{Agent, AgentEvent, AgentOptions, PromptInput};
use pi_ai::{AssistantMessage, Message, StopReason, ThinkingLevel};
use pi_env::EnvConnection;
use tokio_util::sync::CancellationToken;

use super::compaction::{self, CompactionOutcome};
use super::event_bus::{EventBus, EventBusWeak};
use super::model_registry::ModelRegistry;
use super::retry as retry_policy;
use super::session_manager::{self, EntryKind, SessionEntry, SessionManager};
use super::settings_manager::CompactionSettings;

/// Where a fork re-runs from — upstream `fork(entryId, {position})` with
/// `"before"` (re-edit the message) or `"at"` (clone up to the entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
	/// Fork *before* the entry: the entry must be a user message; the new
	/// branch roots at its parent and the message text comes back as
	/// `selectedText` for pre-filling.
	Before,
	/// Fork *at* the entry: the new branch roots at the entry itself.
	At,
}

/// Commands a mode sends to the session task.
#[derive(Debug, Clone)]
pub enum SessionCommand {
	/// New prompt; queued when a run is active.
	Prompt {
		text: String,
	},
	/// Inject mid-run after the current assistant turn.
	Steer {
		text: String,
	},
	/// Queue to run when the agent would otherwise stop.
	FollowUp {
		text: String,
	},
	Abort,
	SetModel {
		query: String,
	},
	SetThinking {
		level: ThinkingLevel,
	},
	CycleThinking,
	Compact,
	/// Toggle automatic threshold compaction (RPC `set_auto_compaction`).
	SetAutoCompaction {
		enabled: bool,
	},
	/// Toggle auto-retry (RPC `set_auto_retry`).
	SetAutoRetry {
		enabled: bool,
	},
	/// Cancel a pending auto-retry backoff (RPC `abort_retry`).
	AbortRetry,
	/// Start a fresh session (RPC `new_session`); `parent_session` records
	/// the lineage in the new header when set.
	NewSession {
		parent_session: Option<String>,
	},
	/// Replace the session with the log at `path` (RPC `switch_session`).
	SwitchSession {
		path: String,
	},
	/// Fork the session at an entry (RPC `fork`/`clone`).
	Fork {
		entry_id: String,
		position: ForkPosition,
	},
	/// Run a bash command outside the agent loop (RPC `bash`).
	ExecuteBash {
		command: String,
		exclude_from_context: bool,
		/// Opaque RPC correlation id, echoed on the update/done events.
		id: Option<Value>,
	},
	/// Cancel running passthrough bash commands (RPC `abort_bash`).
	AbortBash,
	/// Set the session display name (RPC `set_session_name`).
	SetSessionName {
		name: String,
	},
	Exit,
}

/// Wire-level session events with no [`AgentEvent`]/[`SessionEvent`]
/// analogue — the upstream `AgentSessionEvent` types the RPC mode forwards
/// verbatim (`agent-session.ts:207-232`). Delivered on their own broadcast
/// channel; the TUI/print modes never see them.
#[derive(Debug, Clone)]
pub enum WireEvent {
	/// Streaming output chunk of a passthrough bash command.
	BashExecutionUpdate { id: Option<Value>, delta: String },
	/// A passthrough bash command finished; the RPC layer turns this into the
	/// `bash` response frame.
	BashExecutionDone {
		id: Option<Value>,
		result: BashExecutionResult,
	},
	/// The session display name changed (`session_info_changed` upstream);
	/// `None` clears the name and is omitted from the wire frame.
	SessionInfoChanged { name: Option<String> },
	/// `auto_retry_start`: a retryable failure scheduled a retry.
	AutoRetryStart {
		attempt: u32,
		max_attempts: u32,
		delay_ms: u64,
		error_message: String,
	},
	/// `auto_retry_end`: the retry sequence finished (successfully on the
	/// next assistant response, or cancelled/failed).
	AutoRetryEnd {
		success: bool,
		attempt: u32,
		final_error: Option<String>,
	},
}

/// Upstream `BashResult` (`core/bash-executor.ts:27`) — the `bash` command's
/// response payload.
#[derive(Debug, Clone, PartialEq)]
pub struct BashExecutionResult {
	pub output: String,
	/// `None` when the command was killed/cancelled.
	pub exit_code: Option<i64>,
	pub cancelled: bool,
	pub truncated: bool,
	pub full_output_path: Option<String>,
}

impl BashExecutionResult {
	/// Serialize to the upstream wire shape (`exitCode` stays null when
	/// absent; `fullOutputPath` is omitted when absent).
	pub fn to_value(&self) -> Value {
		let mut value = json!({
			"output": self.output,
			"exitCode": self.exit_code,
			"cancelled": self.cancelled,
			"truncated": self.truncated,
		});
		if let Some(path) = &self.full_output_path {
			value["fullOutputPath"] = Value::String(path.clone());
		}
		value
	}
}

/// Events the session emits to modes.
///
/// Compaction reports through `Notice` (upstream has dedicated
/// `compaction_start`/`compaction_end` session events; modes here match
/// `SessionEvent` exhaustively, so a dedicated variant lands with the phase
/// that owns them). Notices use stable prefixes: `compacted:`,
/// `compaction failed:`, `cannot compact:`.
///
/// Upstream `AgentSessionEvent` types that carry no [`AgentEvent`] analogue
/// (`bash_execution_update`, `session_info_changed`, `auto_retry_start/end`)
/// travel on the separate [`WireEvent`] channel instead, so the exhaustive
/// `SessionEvent` matches in the TUI/print modes stay closed.
#[derive(Debug, Clone)]
pub enum SessionEvent {
	Agent(Box<AgentEvent>),
	/// Human-facing status/notices (model switched, errors, stub feedback).
	Notice(String),
	/// Persisted a session entry (for `/export` etc.).
	Persisted {
		kind: &'static str,
	},
	/// The session reached a quiescent state: no active run, no in-flight
	/// compaction, empty prompt queue (upstream `agent_settled` — the RPC
	/// client's lifecycle anchor for "no auto-continuation is pending").
	///
	/// `aborted` mirrors upstream `agent_settled.aborted` — whether the
	/// settled run ended because the user aborted it (`_agentRunAbortRequested`).
	Settled {
		aborted: bool,
	},
}

/// Session log plus the persist cursor, shared between the session task and
/// the mid-run compaction hook. Every append goes through one lock, which is
/// never held across an `await` (appends are synchronous fsync'd writes).
///
/// The RPC layer reads the live session through `SharedLogRef` (entries,
/// tree, leaf, name); the task swaps `session` in place on session
/// replacement, so readers always observe the current log.
pub(crate) struct SharedLog {
	pub(crate) session: SessionManager,
	/// Transcript prefix already written to the log (the agent transcript
	/// index; the leading system message is never logged, so the cursor
	/// starts past it).
	pub(crate) persisted: usize,
	/// Compaction summarized + committed to the log by the mid-run hook,
	/// waiting for the run to end so the transcript swap becomes legal.
	pub(crate) pending_mid_run: Option<PendingMidRunCompaction>,
	/// Bumped on every session replacement. Bash jobs capture it at spawn
	/// and drop their results when the log moved on (the result belongs to
	/// the outgoing session).
	pub(crate) generation: u64,
	/// Passthrough bash results that completed while the transcript was not
	/// fully persisted; flushed at run boundaries (upstream
	/// `_pendingBashMessages` + `_flushPendingBashMessages`).
	pub(crate) pending_bash: Vec<PendingBashRecord>,
}

/// A bash result waiting for the transcript to settle before it can join the
/// live context (`BashExecutionMessage` upstream).
pub(crate) struct PendingBashRecord {
	pub(crate) entry: SessionEntry,
	/// The user-form text the non-excluded entry contributes to context.
	pub(crate) context_text: Option<String>,
}

pub(crate) type SharedLogRef = Arc<std::sync::Mutex<SharedLog>>;

/// State shared between the session task and the mode handles. The task
/// updates the slots in place on session replacement; probes read them
/// without round-tripping a command.
pub(crate) struct SessionShared {
	/// The live agent (swapped on session replacement).
	pub(crate) live_agent: Arc<std::sync::Mutex<Agent>>,
	/// The live session file (`…/session.jsonl`); `None` while unpersisted.
	pub(crate) current_session_file: Arc<std::sync::Mutex<Option<PathBuf>>>,
	/// The live session display name (latest `session_info`).
	pub(crate) session_name: Arc<std::sync::Mutex<Option<String>>>,
	/// Wire-level events (bash updates, retry lifecycle).
	pub(crate) wire_events: tokio::sync::broadcast::Sender<WireEvent>,
	/// The `willRetry` decision per forwarded `agent_end`, in order. The task
	/// pushes before forwarding; the RPC layer pops when it serializes the
	/// event, so the pairing survives broadcast buffering. (A lagged RPC
	/// receiver can drop an `agent_end` while its decision stays queued —
	/// the flag is advisory and self-corrects on the next run.)
	pub(crate) agent_end_will_retry: Arc<std::sync::Mutex<VecDeque<bool>>>,
	/// Auto-retry runtime state (attempt counter + effective base delay).
	pub(crate) retry: Arc<std::sync::Mutex<RetryRuntime>>,
}

/// The auto-retry attempt counter plus the effective backoff base. The
/// shipped settings schema has no retry key yet, so the base defaults to
/// `retry::RETRY_BASE_DELAY_MS`; tests shorten it through this slot.
#[derive(Debug)]
pub(crate) struct RetryRuntime {
	pub(crate) attempt: u32,
	pub(crate) base_delay_ms: u64,
}

impl SessionShared {
	fn publish_session(&self, session: &SessionManager, agent: &Agent) {
		*self.live_agent.lock().unwrap() = agent.clone();
		*self.current_session_file.lock().unwrap() =
			session.session_dir().map(|dir| dir.join("session.jsonl"));
		*self.session_name.lock().unwrap() = session.session_name();
	}

	fn emit(&self, event: WireEvent) {
		let _ = self.wire_events.send(event);
	}
}

/// See [`SharedLog::pending_mid_run`].
pub(crate) struct PendingMidRunCompaction {
	pub(crate) outcome: CompactionOutcome,
}

/// Handle held by modes; the session task runs in the background.
pub struct SessionHandle {
	pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
	pub events: tokio::sync::broadcast::Receiver<SessionEvent>,
	/// The startup agent, kept for direct status probes in the TUI/print
	/// modes (they never trigger session replacement). RPC reads the live
	/// agent through [`SessionHandle::live_agent`] instead.
	pub agent: Agent,
	pub session_dir: Option<std::path::PathBuf>,
	/// Mirrors the auto-compaction toggle for lock-free status probes (TUI
	/// footer indicator); the session task owns the authoritative state.
	pub auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	/// True while a compaction summarization is in flight (RPC `get_state`'s
	/// `isCompacting`); the session task owns the authoritative state.
	pub compacting: Arc<std::sync::atomic::AtomicBool>,
	/// The live session log — swapped by the session task on session
	/// replacement, so probes always observe the current session.
	pub(crate) log: SharedLogRef,
	/// Shared live-session slots ([`SessionShared`]).
	pub(crate) shared: Arc<SessionShared>,
	/// Wire-level events (bash updates, retry lifecycle), pre-subscribed.
	pub wire_events: tokio::sync::broadcast::Receiver<WireEvent>,
	/// Auto-retry toggle (runtime state; the settings schema has no retry
	/// key yet — see `core/retry.rs`).
	pub(crate) auto_retry: Arc<std::sync::atomic::AtomicBool>,
}

/// Build and spawn the session task.
pub fn spawn_agent_session(
	agent: Agent,
	session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
) -> SessionHandle {
	spawn_agent_session_with_settings(agent, session, registry, CompactionSettings::default())
}

pub fn spawn_agent_session_with_settings(
	agent: Agent,
	mut session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	settings: CompactionSettings,
) -> SessionHandle {
	let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
	let event_bus = EventBus::<SessionEvent>::new();
	let (wire_tx, wire_rx) = tokio::sync::broadcast::channel(1024);

	let session_dir = session.session_dir().map(|dir| dir.to_path_buf());
	if let Some(dir) = session.session_dir() {
		restore_usage_boundary(&agent, dir);
	}
	agent.set_session_id(session.session_id());
	record_initial_model_entries(&agent, &mut session);
	let auto_compaction = Arc::new(std::sync::atomic::AtomicBool::new(
		settings.enabled.unwrap_or(true),
	));
	let compacting = Arc::new(std::sync::atomic::AtomicBool::new(false));

	let log: SharedLogRef = Arc::new(std::sync::Mutex::new(SharedLog {
		persisted: agent.snapshot().messages.len(),
		session,
		pending_mid_run: None,
		generation: 0,
		pending_bash: Vec::new(),
	}));

	let shared = Arc::new(SessionShared {
		live_agent: Arc::new(std::sync::Mutex::new(agent.clone())),
		current_session_file: Arc::new(std::sync::Mutex::new(
			session_dir.as_ref().map(|dir| dir.join("session.jsonl")),
		)),
		session_name: Arc::new(std::sync::Mutex::new(
			log.lock().unwrap().session.session_name(),
		)),
		wire_events: wire_tx,
		agent_end_will_retry: Arc::new(std::sync::Mutex::new(VecDeque::new())),
		retry: Arc::new(std::sync::Mutex::new(RetryRuntime {
			attempt: 0,
			base_delay_ms: retry_policy::RETRY_BASE_DELAY_MS,
		})),
	});
	shared.publish_session(&log.lock().unwrap().session, &agent);

	// Mid-run compaction (upstream `prepareNextTurn` between-turn check):
	// installed before the task starts so every run of this session carries
	// it. The closure clones the shared state; see [`MidRunHook::check`].
	install_mid_run_hook(
		&agent,
		log.clone(),
		auto_compaction.clone(),
		compacting.clone(),
		settings.clone(),
		event_bus.downgrade(),
	);

	let auto_retry = Arc::new(std::sync::atomic::AtomicBool::new(
		retry_policy::RETRY_ENABLED_DEFAULT,
	));

	tokio::spawn(session_task(
		agent.clone(),
		log.clone(),
		registry,
		cmd_rx,
		event_bus.clone(),
		auto_compaction.clone(),
		compacting.clone(),
		settings,
		shared.clone(),
		auto_retry.clone(),
	));

	SessionHandle {
		cmd_tx,
		events: event_bus.subscribe(),
		agent,
		session_dir,
		auto_compaction,
		compacting,
		log,
		shared,
		wire_events: wire_rx,
		auto_retry,
	}
}

/// Resume-time usage trust boundary: persisted usage measured before the
/// newest compaction is distrusted (see [`session_manager::SessionProjection`]
/// and `context_usage.rs`).
fn restore_usage_boundary(agent: &Agent, dir: &std::path::Path) {
	let projection = session_manager::project_file(dir);
	if projection.usage_valid_from > 0 {
		let snapshot = agent.snapshot();
		let ordinary: Vec<_> = snapshot
			.messages
			.iter()
			.filter(|m| !matches!(m, Message::System(_)))
			.collect();
		if ordinary.len() == projection.messages.len()
			&& ordinary
				.iter()
				.zip(&projection.messages)
				.all(|(a, (_, b))| message_projection_eq(a, b))
		{
			let leading_systems = snapshot.messages.len() - ordinary.len();
			agent.set_usage_valid_from(leading_systems + projection.usage_valid_from);
		}
	}
}

/// Upstream records the session's initial model/thinking as entries right
/// after the header (agent-session startup); without them a fresh Rust
/// session lacks the `model_change`/`thinking_level_change` chain the TS pi
/// always writes. Restored sessions keep their existing chain.
fn record_initial_model_entries(agent: &Agent, session: &mut SessionManager) {
	if session.entries().is_empty() {
		let model = agent.model();
		let thinking = serde_json::to_value(agent.thinking_level())
			.ok()
			.and_then(|v| v.as_str().map(str::to_string))
			.unwrap_or_else(|| "off".to_string());
		if let Err(error) = session
			.append(SessionEntry::model_change(
				model.provider.clone(),
				model.id.clone(),
			))
			.and_then(|_| session.append(SessionEntry::thinking_level_change(thinking)))
		{
			eprintln!("warning: failed to record session model/thinking entries: {error}");
		}
	}
}

/// Install the between-turn compaction hook on an agent (rebuilt on session
/// replacement, since the hook captures the agent it compacts for).
fn install_mid_run_hook(
	agent: &Agent,
	log: SharedLogRef,
	auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	compacting: Arc<std::sync::atomic::AtomicBool>,
	settings: CompactionSettings,
	bus: EventBusWeak<SessionEvent>,
) {
	let hook = Arc::new(MidRunHook {
		agent: agent.clone(),
		log,
		auto_compaction,
		compacting,
		settings,
		bus,
	});
	agent.set_prepare_next_turn(Arc::new(move |_messages| {
		let hook = hook.clone();
		Box::pin(async move {
			hook.check().await;
			None
		})
	}));
}

/// The `prepare_next_turn` hook: upstream's between-turn compaction check
/// (`_compactBeforeNextAssistantResponse` — after tools finish and their
/// results are appended, before the next assistant request; skipped when the
/// batch terminated the run, which the loop expresses by simply not calling
/// the hook). Threshold formula matches the idle paths:
/// `context_tokens > window - reserve`.
struct MidRunHook {
	agent: Agent,
	log: SharedLogRef,
	auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	compacting: Arc<std::sync::atomic::AtomicBool>,
	settings: CompactionSettings,
	/// Weak so the hook — which lives as long as the agent — never keeps the
	/// session-event channel (and with it the session task's lifecycle) open.
	bus: EventBusWeak<SessionEvent>,
}

impl MidRunHook {
	/// Notice through the bus while the session task is alive; a closed bus
	/// (task exited) drops the notice.
	fn emit(&self, event: SessionEvent) {
		if let Some(bus) = self.bus.upgrade() {
			bus.emit(event);
		}
	}

	async fn check(&self) {
		if !self
			.auto_compaction
			.load(std::sync::atomic::Ordering::Relaxed)
		{
			return;
		}
		let Some((tokens, window)) = super::context_usage::compute(&self.agent) else {
			return;
		};
		if tokens <= window.saturating_sub(self.settings.reserve_tokens.unwrap_or(16_384)) {
			return;
		}
		if self.log.lock().unwrap().pending_mid_run.is_some() {
			return;
		}
		self.compacting
			.store(true, std::sync::atomic::Ordering::Relaxed);
		self.compact().await;
		// The flag stays raised while a committed compaction waits for its
		// swap; the session task clears it after applying (the loop-top
		// refresh may also clear it — the flag is a status probe only).
		let pending = self.log.lock().unwrap().pending_mid_run.is_some();
		if !pending {
			self.compacting
				.store(false, std::sync::atomic::Ordering::Relaxed);
		}
	}

	/// Summarize past the threshold and commit the compaction entry to the
	/// log. The transcript swap itself must wait for the run to end
	/// (pi-agent refuses replacements mid-run), so the hook marks the log
	/// pending and aborts the run — the session task applies the swap at the
	/// run boundary and resumes with `continue_run`.
	async fn compact(&self) {
		let keep_recent = self.settings.keep_recent_tokens.unwrap_or(20_000);
		// Persist whatever the run already produced and select the cut under
		// one lock; no awaits while it is held.
		let cut = {
			let mut guard = self.log.lock().unwrap();
			if let Some(bus) = self.bus.upgrade() {
				persist_messages(&self.agent, &mut guard, &bus);
			}
			auto_compaction_cut(&self.agent, &mut guard, keep_recent)
		};
		let Some(cut) = cut else {
			return;
		};
		self.emit(SessionEvent::Notice("auto-compaction (mid-run)…".into()));
		match compaction::compact_prefix(&self.agent, Some(cut)).await {
			Ok(outcome) => {
				let mut guard = self.log.lock().unwrap();
				// The log entry leads the swap (upstream
				// `SessionManager.appendCompaction`): a crash between entry
				// and swap still reconciles through the projection.
				let mut entry =
					SessionEntry::compaction(outcome.summary.clone(), outcome.tokens_before);
				if let (
					Some(id),
					EntryKind::Compaction {
						first_kept_entry_id,
						..
					},
				) = (&outcome.first_kept_id, &mut entry.kind)
				{
					*first_kept_entry_id = id.clone();
				}
				match guard.session.append(entry) {
					Ok(_) => {
						guard.pending_mid_run = Some(PendingMidRunCompaction { outcome });
					}
					Err(error) => self.emit(SessionEvent::Notice(format!(
						"session write failed: {error}"
					))),
				}
			}
			Err(error) => self.emit(SessionEvent::Notice(format!("compaction failed: {error}"))),
		}
		if self.log.lock().unwrap().pending_mid_run.is_some() {
			// End the run at the loop boundary; the task applies the swap and
			// continues the interrupted work.
			self.agent.abort();
		}
	}
}

#[allow(clippy::too_many_arguments)] // the flag pair and settings are distinct inputs
async fn session_task(
	mut agent: Agent,
	log: SharedLogRef,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
	event_bus: EventBus<SessionEvent>,
	auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	compacting_flag: Arc<std::sync::atomic::AtomicBool>,
	settings: CompactionSettings,
	shared: Arc<SessionShared>,
	auto_retry: Arc<std::sync::atomic::AtomicBool>,
) {
	let mut agent_rx = agent.subscribe();
	let mut pending = VecDeque::new();
	let mut run: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
	// In-flight compaction summarization; the agent is idle while this runs.
	let mut compacting: Option<tokio::task::JoinHandle<anyhow::Result<CompactionOutcome>>> = None;
	let mut compaction_commit_failed = false;
	let mut recovery_compaction = false;
	let mut overflow_attempted = false;
	// Set when the user aborts the active run; suppresses the auto-retry
	// prediction for that run's `agent_end` (upstream `_agentRunAbortRequested`).
	let mut abort_requested = false;
	// Cancellation token for a pending retry backoff (`_retryAbortController`).
	let mut retry_cancel: Option<CancellationToken> = None;
	// Abort tokens of running passthrough bash commands (`_bashAbortControllers`).
	let mut bash_tokens: Vec<CancellationToken> = Vec::new();
	// Settle tracking: `agent_settled` fires on each busy→idle transition;
	// the initial idle state does not emit (nothing has settled yet).
	let mut was_idle = true;
	let mut first_loop = true;

	let notice = |bus: &EventBus<SessionEvent>, text: String| {
		bus.emit(SessionEvent::Notice(text));
	};

	loop {
		// Derived probes, refreshed each iteration before awaiting: the flag
		// updates lag a spawn by one loop pass at most (microseconds — the
		// select re-loops immediately after an arm runs).
		let mid_run_pending = log.lock().unwrap().pending_mid_run.is_some();
		compacting_flag.store(
			compacting.is_some() || mid_run_pending,
			std::sync::atomic::Ordering::Relaxed,
		);
		let idle = run.is_none() && compacting.is_none() && pending.is_empty();
		if first_loop {
			first_loop = false;
		}
		// (rpc-mode P1, agent D) `agent_settled` must follow the finished
		// run's buffered agent events on the wire (upstream emits it only
		// after the run loop fully unwound), so the settle is deferred until
		// agent_rx has been drained. The run-completion branch itself still
		// runs ahead of the buffered events — recovery triggers fire before
		// they are re-emitted — only the settle waits for the drain.
		let settled = idle && agent_rx.is_empty();
		if settled && !was_idle {
			event_bus.emit(SessionEvent::Settled {
				aborted: abort_requested,
			});
		}
		was_idle = settled;

		tokio::select! {
			biased;

			command = cmd_rx.recv() => {
				let Some(command) = command else { break };
				match command {
					SessionCommand::Prompt { text } => {
						if run.is_some() || agent.is_streaming() || compacting.is_some() {
							pending.push_back(text);
							if compacting.is_some() {
								notice(&event_bus, "compaction running — prompt queued".into());
							} else {
								notice(&event_bus, "run active — prompt queued".into());
							}
						} else if auto_compaction.load(std::sync::atomic::Ordering::Relaxed)
							&& let Some((tokens, window)) = super::context_usage::compute(&agent)
							&& tokens > window.saturating_sub(settings.reserve_tokens.unwrap_or(16_384))
							&& let Some(cut) = auto_compaction_cut(&agent, &mut log.lock().unwrap(), settings.keep_recent_tokens.unwrap_or(20_000)) {
							pending.push_back(text);
							notice(&event_bus, "auto-compaction (threshold)…".into());
							let agent = agent.clone();
							compacting = Some(tokio::spawn(async move { compaction::compact_prefix(&agent, Some(cut)).await }));
						} else {
							overflow_attempted = false;
							abort_requested = false;
							let agent = agent.clone();
							run = Some(tokio::spawn(async move {
								agent.prompt(PromptInput::Text { text }).await
							}));
						}
					}
					SessionCommand::Steer { text } => {
						agent.steer(text);
						notice(&event_bus, "steering message queued".into());
					}
					SessionCommand::FollowUp { text } => {
						agent.follow_up(text);
						notice(&event_bus, "follow-up message queued".into());
					}
					SessionCommand::Abort => {
						recovery_compaction = false;
						abort_requested = true;
						let stopped_compaction = if let Some(task) = compacting.take() {
							task.abort();
							let _ = task.await;
							true
						} else { false };
						agent.abort();
						// A pending retry backoff dies with the run
						// (upstream `_finishCancelledRetry` on abort).
						if let Some(token) = retry_cancel.take() {
							token.cancel();
						} else {
							finish_cancelled_retry(&shared);
						}
						notice(&event_bus, "aborting…".into());
						if stopped_compaction && run.is_none() && !agent.is_streaming()
							&& let Some(text) = pending.pop_front() {
							overflow_attempted = false;
							abort_requested = false;
							let agent = agent.clone();
							run = Some(tokio::spawn(async move { agent.prompt(PromptInput::Text { text }).await }));
						}
					}
					SessionCommand::SetModel { query } => {
						let found = registry.lock().unwrap().find(&query);
						match found {
							Some(model) => {
								agent.set_stream_fn(super::model_registry::authed_stream_fn(
									registry.clone(),
								));
								agent.set_model(model.clone());
								registry.lock().unwrap().set_default(&model);
								let change = SessionEntry::model_change(
									model.provider.clone(),
									model.id.clone(),
								);
								let write = log.lock().unwrap().session.append(change);
								if let Err(e) = write {
									notice(&event_bus, format!("session write failed: {e}"));
								}
								notice(&event_bus, format!("model → {}", model.key()));
							}
							None => notice(&event_bus, format!("no model matching `{query}`")),
						}
					}
					SessionCommand::SetThinking { level } => {
						agent.set_thinking_level(level);
						persist_thinking_level(&log, level, &event_bus);
						notice(&event_bus, format!("thinking → {level:?}"));
					}
					SessionCommand::CycleThinking => {
						let next = next_thinking_level(agent.thinking_level());
						agent.set_thinking_level(next);
						persist_thinking_level(&log, next, &event_bus);
						notice(&event_bus, format!("thinking → {next:?}"));
					}
					SessionCommand::Compact => {
						// A finished-but-unreaped run is select-order lag, not an
						// active run; reap it so acceptance reflects the agent.
						if run.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
							if let Some(task) = run.as_mut() {
								match task.await {
									Ok(Ok(())) => {},
									Ok(Err(e)) => notice(&event_bus, format!("prompt failed: {e}")),
									Err(e) => notice(&event_bus, format!("prompt task failed: {e}")),
								}
							}
							run = None;
							let mut guard = log.lock().unwrap();
							persist_messages(&agent, &mut guard, &event_bus);
						}
						if run.is_some() || agent.is_streaming() {
							// Upstream aborts the run before compacting; this port
							// refuses instead so a slash command never discards an
							// in-flight turn.
							notice(&event_bus, "cannot compact: agent run active".into());
						} else if compacting.is_some() {
							notice(&event_bus, "compaction already running".into());
						} else {
							let fully_persisted = {
								let mut guard = log.lock().unwrap();
								persist_messages(&agent, &mut guard, &event_bus);
								guard.persisted == agent.snapshot().messages.len()
							};
							if !fully_persisted {
								notice(&event_bus, "cannot compact: transcript is not fully persisted".into());
								continue;
							}
							notice(&event_bus, "compacting…".into());
							let agent = agent.clone();
							compacting = Some(tokio::spawn(async move {
								compaction::compact_agent(&agent).await
							}));
						}
					}
					SessionCommand::SetAutoCompaction { enabled } => {
						auto_compaction.store(enabled, std::sync::atomic::Ordering::Relaxed);
						notice(
							&event_bus,
							if enabled {
								"auto-compaction enabled".into()
							} else {
								"auto-compaction disabled".into()
							},
						);
					}
					SessionCommand::SetAutoRetry { enabled } => {
						auto_retry.store(enabled, std::sync::atomic::Ordering::Relaxed);
						notice(
							&event_bus,
							if enabled {
								"auto-retry enabled".into()
							} else {
								"auto-retry disabled".into()
							},
						);
					}
					SessionCommand::AbortRetry => {
						// Cancel a pending backoff; the backoff task reports
						// the cancellation itself when one is in flight.
						if let Some(token) = retry_cancel.take() {
							token.cancel();
						} else {
							finish_cancelled_retry(&shared);
						}
					}
					SessionCommand::NewSession { parent_session } => {
						match plan_new_session(&log, parent_session) {
							Ok(plan) => {
								let (new_agent, new_rx) = apply_replacement(ReplacementContext {
									agent: &agent,
									log: &log,
									event_bus: &event_bus,
									auto_compaction: &auto_compaction,
									compacting_flag: &compacting_flag,
									compacting: &mut compacting,
									run: &mut run,
									agent_rx: &mut agent_rx,
									pending: &mut pending,
									abort_requested: &mut abort_requested,
									overflow_attempted: &mut overflow_attempted,
									recovery_compaction: &mut recovery_compaction,
									retry_cancel: &mut retry_cancel,
									shared: &shared,
									settings: &settings,
									plan,
								})
								.await;
								agent = new_agent;
								agent_rx = new_rx;
							}
							Err(error) => notice(&event_bus, error),
						}
					}
					SessionCommand::SwitchSession { path } => {
						match plan_switch_session(&path) {
							Ok(plan) => {
								let (new_agent, new_rx) = apply_replacement(ReplacementContext {
									agent: &agent,
									log: &log,
									event_bus: &event_bus,
									auto_compaction: &auto_compaction,
									compacting_flag: &compacting_flag,
									compacting: &mut compacting,
									run: &mut run,
									agent_rx: &mut agent_rx,
									pending: &mut pending,
									abort_requested: &mut abort_requested,
									overflow_attempted: &mut overflow_attempted,
									recovery_compaction: &mut recovery_compaction,
									retry_cancel: &mut retry_cancel,
									shared: &shared,
									settings: &settings,
									plan,
								})
								.await;
								agent = new_agent;
								agent_rx = new_rx;
							}
							Err(error) => notice(&event_bus, error),
						}
					}
					SessionCommand::Fork { entry_id, position } => {
						match plan_fork(&log, &entry_id, position) {
							Ok((plan, _selected_text)) => {
								let (new_agent, new_rx) = apply_replacement(ReplacementContext {
									agent: &agent,
									log: &log,
									event_bus: &event_bus,
									auto_compaction: &auto_compaction,
									compacting_flag: &compacting_flag,
									compacting: &mut compacting,
									run: &mut run,
									agent_rx: &mut agent_rx,
									pending: &mut pending,
									abort_requested: &mut abort_requested,
									overflow_attempted: &mut overflow_attempted,
									recovery_compaction: &mut recovery_compaction,
									retry_cancel: &mut retry_cancel,
									shared: &shared,
									settings: &settings,
									plan,
								})
								.await;
								agent = new_agent;
								agent_rx = new_rx;
							}
							Err(error) => notice(&event_bus, error),
						}
					}
					SessionCommand::ExecuteBash { command, exclude_from_context, id } => {
						let (cwd, session_env, generation) = {
							let guard = log.lock().unwrap();
							let cwd = session_cwd(&guard.session);
							let session_env = bash_session_env_vars(
								&guard.session,
								&agent.model(),
								&agent.thinking_level(),
							);
							(cwd, session_env, guard.generation)
						};
						let token = CancellationToken::new();
						bash_tokens.push(token.clone());
						tokio::spawn(run_bash_job(BashJob {
							id,
							command,
							exclude_from_context,
							token,
							generation,
							cwd,
							session_env,
							log: log.clone(),
							agent: agent.clone(),
							bus: event_bus.clone(),
							shared: shared.clone(),
						}));
					}
					SessionCommand::AbortBash => {
						for token in bash_tokens.drain(..) {
							token.cancel();
						}
					}
					SessionCommand::SetSessionName { name } => {
						let result = {
							let mut guard = log.lock().unwrap();
							guard.session.append_session_info(&name)
						};
						match result {
							Ok(_) => {
								let current = {
									let guard = log.lock().unwrap();
									let name = guard.session.session_name();
									*shared.session_name.lock().unwrap() = name.clone();
									name
								};
								shared.emit(WireEvent::SessionInfoChanged { name: current });
							}
							Err(error) => notice(&event_bus, format!("session write failed: {error}")),
						}
					}
					SessionCommand::Exit => break,
				}
			}
			result = async { run.as_mut().expect("guarded run").await }, if run.is_some() => {
				run = None;
				// The backoff token of a retry run is dead once the run ends;
				// a cancelled backoff took it before aborting the task.
				retry_cancel = None;
				match result {
					Ok(Ok(())) => {},
					Ok(Err(e)) => notice(&event_bus, format!("prompt failed: {e}")),
					Err(e) => notice(&event_bus, format!("prompt task failed: {e}")),
				}
				// Persist the completed turn before selecting a durable cut.
				let mut guard = log.lock().unwrap();
				persist_messages(&agent, &mut guard, &event_bus);
				drop(guard);

				// Mid-run compaction committed by the hook: the run was cut
				// short so the transcript swap could happen (pi-agent refuses
				// replacements while a run is active). Apply it now and resume
				// the interrupted work.
				if log.lock().unwrap().pending_mid_run.is_some() {
					if apply_mid_run_compaction(&agent, &log, &event_bus) {
						let last_is_assistant = agent
							.snapshot()
							.messages
							.last()
							.is_some_and(|m| m.role() == "assistant");
						if !last_is_assistant && !agent.is_streaming() {
							let agent = agent.clone();
							run = Some(tokio::spawn(async move { agent.continue_run().await }));
						}
					} else {
						compaction_commit_failed = true;
						notice(&event_bus, "session stopped after compaction commit failure; reopen to recover".into());
						break;
					}
				}

				let snapshot = agent.snapshot();
				let recover = auto_compaction.load(std::sync::atomic::Ordering::Relaxed)
					&& snapshot.messages.last().is_some_and(|m| matches!(m, Message::Assistant(a) if recoverable_overflow(a, &agent.model())));
				if recover && !overflow_attempted {
					// Omit first, then select the cut: the failed attempt is
					// already context-invisible, so the boundary reflects the
					// post-omission transcript — the port's form of upstream's
					// recovery-omission suffix advancement
					// (`findProjectedCutPoint` skips entries the omission made
					// invisible; here they are gone from the array entirely).
					match omit_failed_attempt(&agent, &log) {
						Ok(()) => {
							let cut = auto_compaction_cut(&agent, &mut log.lock().unwrap(), settings.keep_recent_tokens.unwrap_or(20_000));
							if let Some(cut) = cut {
								overflow_attempted = true;
								recovery_compaction = true;
								notice(&event_bus, "context overflow: compacting before one retry".into());
								let agent = agent.clone();
								compacting = Some(tokio::spawn(async move { compaction::compact_prefix(&agent, Some(cut)).await }));
							} else {
								notice(&event_bus, "overflow recovery unavailable: no durable compaction boundary".into());
							}
						}
						Err(error) => {
							compaction_commit_failed = true;
							notice(&event_bus, format!("overflow recovery commit failed: {error}; reopen to recover"));
							break;
						}
					}
				} else if recover && overflow_attempted {
					notice(&event_bus, "overflow recovery failed after one compact-and-retry attempt".into());
				}
				if !recover && auto_compaction.load(std::sync::atomic::Ordering::Relaxed)
					&& let Some((tokens, window)) = super::context_usage::compute(&agent)
					&& tokens > window.saturating_sub(settings.reserve_tokens.unwrap_or(16_384))
					&& let Some(cut) = auto_compaction_cut(&agent, &mut log.lock().unwrap(), settings.keep_recent_tokens.unwrap_or(20_000))
				{
					notice(&event_bus, "auto-compaction (threshold)…".into());
					let agent = agent.clone();
					compacting = Some(tokio::spawn(async move { compaction::compact_prefix(&agent, Some(cut)).await }));
				}
				// Auto-retry (upstream `_handlePostAgentRun` + `_prepareRetry`):
				// a run that ended in a retryable error schedules one repeat
				// after an exponential backoff; the failed attempt is omitted
				// from the model context exactly like the overflow recovery.
				// Overflow and recoverable-length stops never reach the retry —
				// they are excluded from retryability (upstream
				// `_isRetryableError`), so the two recoveries never stack.
				let last_assistant = agent.snapshot().messages.iter().rev().find_map(|message| match message {
					Message::Assistant(assistant) => Some(assistant.clone()),
					_ => None,
				});
				let retry_enabled = auto_retry.load(std::sync::atomic::Ordering::Relaxed);
				let will_retry = !abort_requested
					&& retry_enabled
					&& last_assistant.as_ref().is_some_and(|assistant| {
						retry_policy::is_retryable_error(assistant, agent.model().context_window)
					})
					&& shared.retry.lock().unwrap().attempt < retry_policy::RETRY_MAX_RETRIES;
				if will_retry && compacting.is_none() && run.is_none() && !agent.is_streaming() {
					match omit_failed_attempt(&agent, &log) {
						Ok(()) => {
							let (attempt, delay_ms) = {
								let mut guard = shared.retry.lock().unwrap();
								guard.attempt += 1;
								let delay =
									retry_policy::retry_delay_with(guard.base_delay_ms, guard.attempt);
								(guard.attempt, delay)
							};
							shared.emit(WireEvent::AutoRetryStart {
								attempt,
								max_attempts: retry_policy::RETRY_MAX_RETRIES,
								delay_ms,
								error_message: last_assistant
									.as_ref()
									.and_then(|assistant| assistant.error_message.clone())
									.unwrap_or_else(|| "Unknown error".into()),
							});
							let cancel = CancellationToken::new();
							retry_cancel = Some(cancel.clone());
							let agent = agent.clone();
							let shared = shared.clone();
							run = Some(tokio::spawn(async move {
								tokio::select! {
									_ = cancel.cancelled() => {
										let attempt = {
											let mut guard = shared.retry.lock().unwrap();
											std::mem::take(&mut guard.attempt)
										};
										if attempt > 0 {
											shared.emit(WireEvent::AutoRetryEnd {
												success: false,
												attempt,
												final_error: Some("Retry cancelled".into()),
											});
										}
										Ok(())
									}
									_ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {
										agent.continue_run().await
									}
								}
							}));
						}
						Err(error) => notice(&event_bus, format!("auto-retry unavailable: {error}")),
					}
				} else if !abort_requested
					&& last_assistant.as_ref().is_some_and(|assistant| assistant.stop_reason == StopReason::Error)
				{
					// The retry budget is spent (or the error is not
					// retryable) after earlier attempts: report the final
					// failure and reset (upstream `_handlePostAgentRun`).
					let attempt = shared.retry.lock().unwrap().attempt;
					if attempt > 0 {
						shared.emit(WireEvent::AutoRetryEnd {
							success: false,
							attempt,
							final_error: last_assistant
								.as_ref()
								.and_then(|assistant| assistant.error_message.clone()),
						});
						shared.retry.lock().unwrap().attempt = 0;
					}
				}
				if compacting.is_none() && run.is_none() && !agent.is_streaming() && let Some(text) = pending.pop_front() {
					overflow_attempted = false;
					abort_requested = false;
					let agent = agent.clone();
					run = Some(tokio::spawn(async move { agent.prompt(PromptInput::Text { text }).await }));
				}
			}

			result = async { compacting.as_mut().expect("guarded compaction").await }, if compacting.is_some() => {
				compacting = None;
				let mut committed = false;
				match result {
					Ok(Ok(outcome)) => {
						if apply_compaction(&agent, &log, &event_bus, outcome) {
							committed = true;
						} else {
							compaction_commit_failed = true;
							notice(&event_bus, "session stopped after compaction commit failure; reopen to recover".into());
							break;
						}
					}
					Ok(Err(e)) => notice(&event_bus, format!("compaction failed: {e}")),
					Err(e) => notice(&event_bus, format!("compaction task failed: {e}")),
				}
				if recovery_compaction {
					recovery_compaction = false;
					if committed {
						let agent = agent.clone();
						run = Some(tokio::spawn(async move { agent.continue_run().await }));
					}
				}
				// Prompts queued while compacting start now that the agent is idle.
				if run.is_none()
					&& !agent.is_streaming()
					&& let Some(text) = pending.pop_front()
				{
					overflow_attempted = false;
					abort_requested = false;
					let agent = agent.clone();
					run = Some(tokio::spawn(async move { agent.prompt(PromptInput::Text { text }).await }));
				}
			}
			event = agent_rx.recv() => {
				match event {
					Ok(agent_event) => {
						if matches!(&agent_event, AgentEvent::MessageEnd { .. } | AgentEvent::AgentEnd { .. }) {
							let mut guard = log.lock().unwrap();
							persist_messages(&agent, &mut guard, &event_bus);
						}
						if let AgentEvent::MessageEnd { message: Message::Assistant(assistant) } = &agent_event {
							// The counter resets on the first non-error
							// assistant response, even mid-run (upstream
							// agent-session.ts:1177).
							if assistant.stop_reason != StopReason::Error {
								let mut retry = shared.retry.lock().unwrap();
								if retry.attempt > 0 {
									let attempt = std::mem::take(&mut retry.attempt);
									drop(retry);
									shared.emit(WireEvent::AutoRetryEnd {
										success: true,
										attempt,
										final_error: None,
									});
								}
							}
						}
						if let AgentEvent::AgentEnd { messages } = &agent_event {
							// willRetry prediction shipped with the `agent_end`
							// wire frame (upstream `_willRetryAfterAgentEnd`).
							// Computed from the run's own message list — the
							// live transcript may already have had the failed
							// attempt omitted for the retry by the time this
							// buffered event is forwarded.
							let will_retry = !abort_requested
								&& auto_retry.load(std::sync::atomic::Ordering::Relaxed)
								&& shared.retry.lock().unwrap().attempt < retry_policy::RETRY_MAX_RETRIES
								&& messages
									.iter()
									.rev()
									.find_map(|message| match message {
										Message::Assistant(assistant) => Some(
											retry_policy::is_retryable_error(
												assistant,
												agent.model().context_window,
											),
										),
										_ => None,
									})
									.unwrap_or(false);
							shared
								.agent_end_will_retry
								.lock()
								.unwrap()
								.push_back(will_retry);
						}
						event_bus.emit(SessionEvent::Agent(Box::new(agent_event)));
					}
					Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
						notice(&event_bus, format!("event lag: dropped {count} events"));
								let mut guard = log.lock().unwrap();
								persist_messages(&agent, &mut guard, &event_bus);
					}
					Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
				}
			}

		}
	}
	if let Some(task) = compacting {
		task.abort();
		let _ = task.await;
	}
	// Stop the active run before taking the final transcript snapshot. A task
	// that has not started yet has no cancellation token, so abort it directly.
	if let Some(mut task) = run {
		if agent.is_streaming() {
			agent.abort();
		} else {
			task.abort();
		}
		if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
			.await
			.is_err()
		{
			task.abort();
			let _ = task.await;
		}
	}
	if !compaction_commit_failed {
		let mut guard = log.lock().unwrap();
		flush_pending_bash(&agent, &mut guard, &event_bus);
		persist_messages(&agent, &mut guard, &event_bus);
	}
}

/// Persist a `thinking_level_change` entry (upstream `appendThinkingLevelChange`)
/// — best effort: a failed write surfaces as a notice, never blocks the toggle.
fn persist_thinking_level(log: &SharedLogRef, level: ThinkingLevel, bus: &EventBus<SessionEvent>) {
	let entry = SessionEntry::thinking_level_change(format!("{level:?}").to_lowercase());
	if let Err(error) = log.lock().unwrap().session.append(entry) {
		bus.emit(SessionEvent::Notice(format!(
			"session write failed: {error}"
		)));
	}
}

// The broadcast channel is lossy; the transcript is the authoritative source.
// Advance only after a successful append so a later event can retry a failed write.
fn persist_messages(agent: &Agent, log: &mut SharedLog, bus: &EventBus<SessionEvent>) {
	if let Err(error) = log.session.sync_pending_metadata() {
		bus.emit(SessionEvent::Notice(format!(
			"session write failed: {error}"
		)));
		return;
	}
	let snapshot = agent.snapshot();
	for message in snapshot.messages.iter().skip(log.persisted) {
		match log.session.append_message(message) {
			Ok(_) => {
				log.persisted += 1;
				if log.session.session_dir().is_some() {
					bus.emit(SessionEvent::Persisted { kind: "message" });
				}
			}
			Err(e) => {
				bus.emit(SessionEvent::Notice(format!("session write failed: {e}")));
				break;
			}
		}
	}
}

/// Reset the retry attempt counter and report the cancellation
/// (upstream `_finishCancelledRetry`). No-op when no attempts are pending.
fn finish_cancelled_retry(shared: &SessionShared) {
	let attempt = {
		let mut guard = shared.retry.lock().unwrap();
		std::mem::take(&mut guard.attempt)
	};
	if attempt > 0 {
		shared.emit(WireEvent::AutoRetryEnd {
			success: false,
			attempt,
			final_error: Some("Retry cancelled".into()),
		});
	}
}

/// The working directory a passthrough bash command runs in: the session
/// header's cwd when persisted, else the process cwd (upstream
/// `sessionManager.getCwd()`).
fn session_cwd(session: &SessionManager) -> PathBuf {
	let from_header = session
		.header()
		.map(|header| header.cwd.clone())
		.filter(|cwd| !cwd.is_empty());
	match from_header {
		Some(cwd) => PathBuf::from(cwd),
		None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
	}
}

/// The `PI_RS_*` session environment passthrough bash commands run with —
/// the same contract as `SessionToolContext::env_vars` in `core/tools/bash.rs`
/// (that builder is private to the tools module, so the passthrough path
/// assembles the identical pairs here).
fn bash_session_env_vars(
	session: &SessionManager,
	model: &pi_ai::Model,
	thinking: &ThinkingLevel,
) -> Vec<(String, String)> {
	let session_file = session
		.session_dir()
		.map(|dir| dir.join("session.jsonl").to_string_lossy().into_owned());
	[
		("PI_RS_SESSION_ID", session.session_id()),
		("PI_RS_SESSION_FILE", session_file),
		("PI_RS_PROVIDER", Some(model.provider.clone())),
		("PI_RS_MODEL", Some(model.id.clone())),
		(
			"PI_RS_REASONING_LEVEL",
			Some(format!("{thinking:?}").to_lowercase()),
		),
	]
	.into_iter()
	.filter_map(|(key, value)| value.map(|value| (key.to_string(), value)))
	.collect()
}

/// Flush bash results that completed while the transcript was not fully
/// persisted (upstream `_flushPendingBashMessages`, run at agent
/// boundaries): persist the entry, then join the live context when eligible.
fn flush_pending_bash(agent: &Agent, log: &mut SharedLog, bus: &EventBus<SessionEvent>) {
	if log.pending_bash.is_empty() {
		return;
	}
	let records = std::mem::take(&mut log.pending_bash);
	for record in records {
		persist_bash_record(agent, log, bus, record);
	}
}

/// Persist one bash record and — when the entry joins the model context —
/// append the user-form text to the live transcript, rebasing the persist
/// cursor so the message is never written twice (the log entry is the
/// `bashExecution` shape; the transcript carries its user-text rendering,
/// and the projection replays them identically).
fn persist_bash_record(
	agent: &Agent,
	log: &mut SharedLog,
	bus: &EventBus<SessionEvent>,
	record: PendingBashRecord,
) {
	if let Err(error) = log.session.append(record.entry) {
		bus.emit(SessionEvent::Notice(format!(
			"session write failed: {error}"
		)));
		return;
	}
	if log.session.session_dir().is_some() {
		bus.emit(SessionEvent::Persisted { kind: "bash" });
	}
	let Some(text) = record.context_text else {
		return; // excludeFromContext: persisted only
	};
	let snapshot = agent.snapshot();
	if agent.is_streaming() || log.persisted != snapshot.messages.len() {
		// Transcript not settleable right now: the log stays authoritative
		// and the context contribution reconciles on resume.
		return;
	}
	let mut messages = snapshot.messages.clone();
	messages.push(Message::user(text));
	if agent.replace_messages(messages).is_ok() {
		log.persisted = agent.snapshot().messages.len();
	}
}

// ---------------------------------------------------------------------------
// Session replacement (`new_session` / `switch_session` / `fork`), port of
// `AgentSessionRuntimeHost` in `core/agent-session-runtime.ts:196-345`.
// ---------------------------------------------------------------------------

/// Everything the runtime needs to build the replacement.
struct ReplacementPlan {
	new_session: SessionManager,
	/// Seed transcript (empty for a fresh session; the target projection for
	/// switch/fork).
	initial_messages: Vec<Message>,
	cwd: PathBuf,
}

/// Resolve `new_session` (upstream `newSession`): a fresh session in the
/// sessions root when the current one persists, else a fresh in-memory one.
/// Deviation: in-memory sessions carry no header, so a requested
/// `parentSession` is not recorded on the in-memory path.
fn plan_new_session(
	log: &SharedLogRef,
	parent_session: Option<String>,
) -> Result<ReplacementPlan, String> {
	let guard = log.lock().unwrap();
	let cwd = session_cwd(&guard.session);
	if guard.session.session_dir().is_some() {
		// Upstream creates the new session in the current sessionDir — the
		// sibling root of the live session, which also keeps tests hermetic.
		let root = guard
			.session
			.session_dir()
			.and_then(Path::parent)
			.map(Path::to_path_buf)
			.unwrap_or_else(crate::config::sessions_dir);
		let new_session = SessionManager::create_with_parent_in(&root, &cwd, parent_session)
			.map_err(|error| format!("cannot create session: {error}"))?;
		Ok(ReplacementPlan {
			new_session,
			initial_messages: Vec::new(),
			cwd,
		})
	} else {
		Ok(ReplacementPlan {
			new_session: SessionManager::in_memory(),
			initial_messages: Vec::new(),
			cwd,
		})
	}
}

/// Resolve `switch_session` (upstream `switchSession`): open the target log,
/// refuse a stored cwd that no longer exists (`assertSessionCwdExists`).
fn plan_switch_session(path: &str) -> Result<ReplacementPlan, String> {
	let target = SessionManager::open_session_path(std::path::Path::new(path))
		.map_err(|error| format!("cannot open session {path}: {error}"))?;
	let cwd = target
		.header()
		.map(|header| header.cwd.clone())
		.filter(|cwd| !cwd.is_empty())
		.ok_or_else(|| format!("session {path} has no cwd in its header"))?;
	if !std::path::Path::new(&cwd).exists() {
		return Err(format!(
			"Stored session working directory does not exist: {cwd}"
		));
	}
	let dir = target
		.session_dir()
		.expect("open() always persists")
		.to_path_buf();
	let initial_messages = restore_initial_messages(&dir);
	Ok(ReplacementPlan {
		new_session: target,
		initial_messages,
		cwd: PathBuf::from(&cwd),
	})
}

/// Resolve `fork` (upstream `fork(entryId, {position})`). `Before` demands a
/// user-message entry and forks from its parent (returning the message text
/// as `selectedText`); `At` forks at the entry. Deviation from upstream:
/// an in-memory session refuses instead of mutating in place — the
/// persisted-file requirement of `runtime.ts:314`.
fn plan_fork(
	log: &SharedLogRef,
	entry_id: &str,
	position: ForkPosition,
) -> Result<(ReplacementPlan, Option<String>), String> {
	let (target_leaf, selected_text, current_file, cwd) = {
		let guard = log.lock().unwrap();
		let entry = guard
			.session
			.get_entry(entry_id)
			.ok_or_else(|| "Invalid entry ID for forking".to_string())?;
		let (target_leaf, selected_text) = match position {
			ForkPosition::At => (entry.id().map(str::to_owned), None),
			ForkPosition::Before => match &entry.kind {
				EntryKind::Message { message } => match message.as_ref() {
					Message::User(user) => (
						entry.parent_id().map(str::to_owned),
						Some(user.content.as_text()),
					),
					_ => return Err("Invalid entry ID for forking".into()),
				},
				_ => return Err("Invalid entry ID for forking".into()),
			},
		};
		let current_file = guard
			.session
			.session_dir()
			.map(|dir| dir.join("session.jsonl"));
		let cwd = session_cwd(&guard.session);
		(target_leaf, selected_text, current_file, cwd)
	};
	if current_file.is_none() {
		return Err(
			"This session has not been saved yet. Send a message before cloning or forking it."
				.into(),
		);
	}
	let plan = match target_leaf {
		None => {
			// Fork before the first entry: a fresh session carrying the
			// parent lineage (runtime.ts:296-303).
			let new_session = SessionManager::create_with_parent(
				&cwd,
				Some(
					current_file
						.expect("checked above")
						.to_string_lossy()
						.into_owned(),
				),
			)
			.map_err(|error| format!("cannot create session: {error}"))?;
			ReplacementPlan {
				new_session,
				initial_messages: Vec::new(),
				cwd,
			}
		}
		Some(target_leaf) => {
			let new_dir = {
				let guard = log.lock().unwrap();
				guard
					.session
					.create_branched_session(&target_leaf)
					.map_err(|error| error.to_string())?
			};
			let new_session = SessionManager::open(&new_dir).map_err(|error| error.to_string())?;
			let initial_messages = restore_initial_messages(&new_dir);
			ReplacementPlan {
				new_session,
				initial_messages,
				cwd,
			}
		}
	};
	Ok((plan, selected_text))
}

/// Mutable task state the replacement consumes (`&mut` bundle — the command
/// arms hold these as locals).
struct ReplacementContext<'a> {
	agent: &'a Agent,
	log: &'a SharedLogRef,
	event_bus: &'a EventBus<SessionEvent>,
	auto_compaction: &'a Arc<std::sync::atomic::AtomicBool>,
	compacting_flag: &'a Arc<std::sync::atomic::AtomicBool>,
	compacting: &'a mut Option<tokio::task::JoinHandle<anyhow::Result<CompactionOutcome>>>,
	run: &'a mut Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
	agent_rx: &'a mut tokio::sync::broadcast::Receiver<AgentEvent>,
	pending: &'a mut VecDeque<String>,
	abort_requested: &'a mut bool,
	overflow_attempted: &'a mut bool,
	recovery_compaction: &'a mut bool,
	retry_cancel: &'a mut Option<CancellationToken>,
	shared: &'a Arc<SessionShared>,
	settings: &'a CompactionSettings,
	plan: ReplacementPlan,
}

/// Tear the current runtime down, build the replacement agent, and swap the
/// task state (`teardownCurrent` → `createRuntime` upstream). The teardown
/// either lands whole or leaves the current session untouched. Returns the
/// new agent and its event receiver for the caller's loop locals.
async fn apply_replacement(
	ctx: ReplacementContext<'_>,
) -> (Agent, tokio::sync::broadcast::Receiver<AgentEvent>) {
	// -- teardown (upstream `teardownCurrent`: abort, settle, persist) -----
	if let Some(task) = ctx.compacting.take() {
		task.abort();
		let _ = task.await;
	}
	if ctx.agent.is_streaming() {
		ctx.agent.abort();
	}
	if let Some(mut task) = ctx.run.take()
		&& tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
			.await
			.is_err()
	{
		task.abort();
		let _ = task.await;
	}
	// A pending retry backoff died with the run (upstream `_finishCancelledRetry`
	// on dispose); the run was aborted above without its cancellation arm.
	finish_cancelled_retry(ctx.shared);
	{
		let mut guard = ctx.log.lock().unwrap();
		flush_pending_bash(ctx.agent, &mut guard, ctx.event_bus);
		persist_messages(ctx.agent, &mut guard, ctx.event_bus);
	}
	// Forward whatever the outgoing agent still has buffered (its final
	// `agent_end` among them) so the wire ordering survives the swap.
	while let Ok(event) = ctx.agent_rx.try_recv() {
		if matches!(
			&event,
			AgentEvent::MessageEnd { .. } | AgentEvent::AgentEnd { .. }
		) {
			let mut guard = ctx.log.lock().unwrap();
			persist_messages(ctx.agent, &mut guard, ctx.event_bus);
		}
		if matches!(&event, AgentEvent::AgentEnd { .. }) {
			ctx.shared
				.agent_end_will_retry
				.lock()
				.unwrap()
				.push_back(false);
		}
		ctx.event_bus.emit(SessionEvent::Agent(Box::new(event)));
	}

	// -- build the replacement runtime -------------------------------------
	let model = ctx.agent.model();
	let thinking = ctx.agent.thinking_level();
	let stream_fn = ctx.agent.stream_fn();
	let (steering_mode, follow_up_mode) = ctx.agent.queue_modes();
	// Tool table: rebuilt built-ins for the (possibly new) cwd, filtered to
	// the startup tool-name set. Deviation: MCP tools (`mcp__*`) cannot be
	// rebuilt here — the MCP assembly is owned by `main.rs` and does not
	// re-run on replacement (upstream re-registers them at `session_start`).
	let startup_names: std::collections::HashSet<String> =
		ctx.agent.snapshot().tool_names.into_iter().collect();
	let tools: Vec<Arc<dyn pi_agent::AgentTool>> = super::tools::create_tools(&ctx.plan.cwd)
		.into_iter()
		.filter(|tool| startup_names.contains(tool.name()))
		.collect();
	// Deviation: the rebuilt system prompt assumes context files enabled —
	// the `--no-context-files` CLI flag is not visible to the session task.
	let system_prompt = super::system_prompt::build_system_prompt(&ctx.plan.cwd, &tools, true);
	let new_agent = Agent::new(AgentOptions {
		model,
		thinking_level: thinking,
		stream_fn: Some(stream_fn),
		tools,
		system_prompt: Some(system_prompt),
		initial_messages: ctx.plan.initial_messages,
		steering_mode,
		follow_up_mode,
		..AgentOptions::default()
	});
	let mut new_session = ctx.plan.new_session;
	record_initial_model_entries(&new_agent, &mut new_session);
	new_agent.set_session_id(new_session.session_id());
	if let Some(dir) = new_session.session_dir() {
		restore_usage_boundary(&new_agent, dir);
	}
	install_mid_run_hook(
		&new_agent,
		ctx.log.clone(),
		ctx.auto_compaction.clone(),
		ctx.compacting_flag.clone(),
		ctx.settings.clone(),
		ctx.event_bus.downgrade(),
	);

	// -- swap the shared state ---------------------------------------------
	{
		let mut guard = ctx.log.lock().unwrap();
		*guard = SharedLog {
			persisted: new_agent.snapshot().messages.len(),
			pending_mid_run: None,
			generation: guard.generation + 1,
			pending_bash: Vec::new(),
			session: new_session,
		};
	}
	ctx.shared
		.publish_session(&ctx.log.lock().unwrap().session, &new_agent);
	let destination = ctx
		.shared
		.current_session_file
		.lock()
		.unwrap()
		.clone()
		.map(|path| path.display().to_string())
		.unwrap_or_else(|| "in-memory".into());
	ctx.event_bus
		.emit(SessionEvent::Notice(format!("session → {destination}")));

	// -- reset the loop-local flags ----------------------------------------
	if !ctx.pending.is_empty() {
		ctx.event_bus.emit(SessionEvent::Notice(format!(
			"discarded {} queued prompt(s) with the replaced session",
			ctx.pending.len()
		)));
	}
	ctx.pending.clear();
	*ctx.abort_requested = false;
	*ctx.overflow_attempted = false;
	*ctx.recovery_compaction = false;
	*ctx.retry_cancel = None;

	let agent_rx = new_agent.subscribe();
	(new_agent, agent_rx)
}

// ---------------------------------------------------------------------------
// Bash passthrough (`executeBash` upstream, `agent-session.ts:3899`)
// ---------------------------------------------------------------------------

/// One running passthrough bash command. Owns the abort token; results are
/// persisted by [`run_bash_job`] when the session generation still matches.
struct BashJob {
	id: Option<Value>,
	command: String,
	exclude_from_context: bool,
	token: CancellationToken,
	generation: u64,
	cwd: PathBuf,
	/// `PI_RS_*` session environment captured at dispatch.
	session_env: Vec<(String, String)>,
	log: SharedLogRef,
	agent: Agent,
	bus: EventBus<SessionEvent>,
	shared: Arc<SessionShared>,
}

/// Execute a passthrough bash command: stream deltas as
/// [`WireEvent::BashExecutionUpdate`], persist the result as the upstream
/// `bashExecution` message entry, join the live context when eligible, and
/// report the final [`BashExecutionResult`]. Runs off the session loop so a
/// long command never blocks event forwarding.
async fn run_bash_job(job: BashJob) {
	let env = pi_env::LocalEnv::with_cwd(job.cwd.to_string_lossy().into_owned());
	// Full output capture for the persisted entry (pi-env's own capture is
	// what the ExecResult carries; this buffer backs the abort report).
	let captured: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
	let capture_sink = captured.clone();
	let on_output: pi_env::connection::OutputCallback = Arc::new(move |chunk: &[u8]| {
		capture_sink
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.extend_from_slice(chunk);
	});
	let chunk_shared = job.shared.clone();
	let chunk_id = job.id.clone();
	let on_chunk: pi_env::connection::OutputChunkCallback = Arc::new(move |chunk: &str| {
		chunk_shared.emit(WireEvent::BashExecutionUpdate {
			id: chunk_id.clone(),
			delta: chunk.to_string(),
		});
	});
	let options = pi_env::ExecOptions {
		cwd: Some(job.cwd.to_string_lossy().into_owned()),
		// Passthrough commands have no timeout — the RPC client decides via
		// `abort_bash` (upstream passes no timeout either).
		timeout_ms: None,
		env: job.session_env.clone(),
		on_output: Some(on_output),
	};

	let started = std::time::Instant::now();
	let outcome = tokio::select! {
		result = env.exec_streaming(&job.command, options, on_chunk) => match result {
			Ok(result) => {
				// Merge like the bash tool's display: stdout, then a stderr
				// section (pi-env captures the streams separately; upstream
				// merges them in arrival order — declared difference).
				let mut output = result.stdout.clone();
				if !result.stderr.is_empty() {
					if !output.is_empty() {
						output.push('\n');
					}
					output.push_str("stderr:\n");
					output.push_str(&result.stderr);
				}
				let spill_paths: Vec<String> = [
					result.full_stdout_path.clone(),
					result.full_stderr_path.clone(),
				]
				.into_iter()
				.flatten()
				.collect();
				let truncated = result.timed_out || !spill_paths.is_empty();
				BashExecutionResult {
					output,
					// A signal-killed shell reports the 128+n convention and
					// no exit code stays None only on cancellation.
					exit_code: result
						.exit_code
						.map(i64::from)
						.or_else(|| result.signal.map(|signal| 128 + i64::from(signal))),
					cancelled: false,
					truncated,
					full_output_path: match spill_paths.is_empty() {
						true => None,
						false => Some(spill_paths.join(", ")),
					},
				}
			}
			Err(error) => {
				let partial = String::from_utf8_lossy(
					&captured.lock().unwrap_or_else(|e| e.into_inner()),
				)
				.into_owned();
				let output = if partial.trim().is_empty() {
					partial
				} else {
					format!("exec failed: {error}\n{partial}")
				};
				job.bus
					.emit(SessionEvent::Notice(format!("bash exec failed: {error}")));
				BashExecutionResult {
					output,
					exit_code: None,
					cancelled: false,
					truncated: false,
					full_output_path: None,
				}
			}
		},
		_ = job.token.cancelled() => {
			let partial = String::from_utf8_lossy(&captured.lock().unwrap_or_else(|e| e.into_inner()))
				.into_owned();
			BashExecutionResult {
				output: partial,
				exit_code: None,
				cancelled: true,
				truncated: false,
				full_output_path: None,
			}
		}
	};
	let _ = started;

	// Persist + context contribution, guarded on the session generation: a
	// replacement moved the log on, and this result belongs to the outgoing
	// session.
	let current_generation = job.log.lock().unwrap().generation;
	if current_generation == job.generation {
		let entry = SessionEntry::new(EntryKind::BashExecution {
			command: job.command.clone(),
			output: outcome.output.clone(),
			exit_code: outcome.exit_code,
			cancelled: outcome.cancelled,
			truncated: outcome.truncated,
			full_output_path: outcome.full_output_path.clone(),
			exclude_from_context: job.exclude_from_context,
		});
		let context_text = (!job.exclude_from_context).then(|| {
			session_manager::bash_execution_to_text(
				&job.command,
				&outcome.output,
				outcome.exit_code,
				outcome.cancelled,
				outcome.truncated,
				outcome.full_output_path.as_deref(),
			)
		});
		let mut guard = job.log.lock().unwrap();
		let fully_persisted = guard.persisted == job.agent.snapshot().messages.len();
		if fully_persisted {
			persist_bash_record(
				&job.agent,
				&mut guard,
				&job.bus,
				PendingBashRecord {
					entry,
					context_text,
				},
			);
		} else {
			// Transcript tail not persisted yet: queue like upstream's
			// `_pendingBashMessages`, flushed at the next run boundary.
			guard.pending_bash.push(PendingBashRecord {
				entry,
				context_text,
			});
		}
	}
	job.shared.emit(WireEvent::BashExecutionDone {
		id: job.id,
		result: outcome,
	});
}

fn message_projection_eq(a: &Message, b: &Message) -> bool {
	let (Ok(mut left), Ok(mut right)) = (serde_json::to_value(a), serde_json::to_value(b)) else {
		return false;
	};
	if let (Some(left), Some(right)) = (left.as_object_mut(), right.as_object_mut()) {
		left.remove("timestamp");
		right.remove("timestamp");
	}
	left == right
}

/// Select a compaction boundary and verify it is durably addressable. The
/// cut may split an oversized single turn (upstream `findProjectedCutPoint`):
/// `cut.turn_start` names the turn's leading user message for the dual
/// summary. Old logs without entry ids are left untouched until they have
/// enough newer ID-bearing entries to form a safe boundary.
fn auto_compaction_cut(
	agent: &Agent,
	log: &mut SharedLog,
	keep_tokens: u64,
) -> Option<compaction::Cut> {
	let snapshot = agent.snapshot();
	if log.persisted != snapshot.messages.len() {
		return None;
	}
	let messages: Vec<Message> = snapshot
		.messages
		.iter()
		.filter(|m| !matches!(m, Message::System(_)))
		.cloned()
		.collect();
	let target = compaction::find_cut_point_detailed(&messages, keep_tokens);
	if target.first_kept == 0 {
		return None;
	}
	let id = if log.session.session_dir().is_some() {
		let projection = log.session.projection();
		if projection.messages.len() != messages.len() {
			return None;
		}
		if !projection
			.messages
			.iter()
			.zip(messages.iter())
			.all(|((_, a), b)| message_projection_eq(a, b))
		{
			return None;
		}
		projection.messages[target.first_kept].0.clone()
	} else {
		None
	};
	Some(compaction::Cut {
		first_kept: target.first_kept,
		turn_start: target.turn_start,
		first_kept_id: id,
	})
}

/// Case-insensitive fragments identifying context-overflow errors — port of
/// `OVERFLOW_PATTERNS` in `pi/packages/ai/src/utils/overflow.ts`, reduced
/// from regexes to literal substrings (upstream's patterns are all fixed
/// phrases around provider overflow wording).
const OVERFLOW_PATTERNS: &[&str] = &[
	"prompt is too long",                    // Anthropic, z.ai
	"prompt too long",                       // Ollama, z.ai CN endpoint
	"prompt exceeds max length",             // z.ai CN endpoint
	"request_too_large",                     // Anthropic HTTP 413 body
	"input is too long for requested model", // Amazon Bedrock
	"exceeds the context window",            // OpenAI
	"maximum context length",                // OpenAI-compatible proxies, OpenRouter, Mistral
	"input token count",                     // Google Gemini
	"exceeds the maximum",                   // Gemini tail, generic
	"maximum prompt length is",              // xAI (Grok)
	"reduce the length of the messages",     // Groq
	"maximum allowed input length",          // OpenRouter/Poolside
	"longer than the model",                 // Together AI
	"exceeds the limit of",                  // GitHub Copilot
	"exceeds the available context size",    // llama.cpp server
	"greater than the context length",       // LM Studio
	"context window exceeds limit",          // MiniMax
	"exceeded model token limit",            // Kimi For Coding
	"too large for model with",              // Mistral
	"configured context size",               // DS4 server
	"model_context_window_exceeded",         // z.ai finish reason
	"range of input length should be",       // DashScope/Qwen
	"context length exceeded",               // generic fallback
	"context_length_exceeded",               // generic fallback
	"context-length-exceeded",               // generic fallback
	"too many tokens",                       // generic fallback
	"token limit exceeded",                  // generic fallback
];

/// Non-overflow errors excluded from overflow detection even when they also
/// match an overflow fragment (`NON_OVERFLOW_PATTERNS` upstream): throttling
/// and rate limiting must not trigger a compaction.
const NON_OVERFLOW_PATTERNS: &[&str] = &[
	"throttling",
	"service unavailable",
	"rate limit",
	"too many requests",
];

/// HTTP 413 means request-size overflow regardless of body text (upstream
/// matches `^[\w -]+ API error \(413\):` and `^413(\s|:|$)`).
fn http_413_overflow(lower: &str) -> bool {
	if let Some(rest) = lower.strip_prefix("413") {
		return rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':');
	}
	lower.contains("api error (413):")
}

/// Whether an assistant message indicates a context overflow — port of
/// `isContextOverflow` (`pi/packages/ai/src/utils/overflow.ts`): an error
/// stop whose message matches a provider overflow pattern, a successful
/// response whose input silently exceeded the window, or a length stop with
/// no room left for output. `pub(crate)`: `core/retry.rs` excludes overflow
/// from retryability (upstream `_isRetryableError`).
pub(crate) fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
	if message.stop_reason == StopReason::Error
		&& let Some(error) = message.error_message.as_deref()
	{
		let lower = error.to_lowercase();
		let non_overflow = NON_OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p));
		if !non_overflow
			&& (OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p)) || http_413_overflow(&lower))
		{
			return true;
		}
	}
	if context_window > 0 {
		let input = message.usage.input.saturating_add(message.usage.cache_read);
		// Silent overflow: the provider accepted the request and answered past
		// the window (z.ai style).
		if message.stop_reason == StopReason::Stop && input > context_window {
			return true;
		}
		// Length stop with zero output: the server truncated oversized input to
		// fill the window (Xiaomi MiMo style), leaving no room to generate.
		if message.stop_reason == StopReason::Length
			&& message.usage.output == 0
			&& input.saturating_mul(100) >= context_window.saturating_mul(99)
		{
			return true;
		}
	}
	false
}

/// Whether a length stop ended below the model's intended output limit, so
/// one bounded compact-and-retry attempt may still finish the turn
/// (`isRecoverableLength` upstream).
fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
	message.stop_reason == StopReason::Length
		&& desired_max_output > 0
		&& message.usage.output < desired_max_output
}

/// One recovery signal for the run-completion branch (`_checkCompaction`
/// upstream): an explicit overflow error or a recoverable length truncation.
/// Aborted messages never reach here — the caller only inspects the tail of a
/// finished run, and user aborts must not compact.
fn recoverable_overflow(message: &AssistantMessage, model: &pi_ai::Model) -> bool {
	is_context_overflow(message, model.context_window)
		|| is_recoverable_length(message, model.max_tokens)
}

/// Persistently drop the failed final attempt from the model context — port
/// of `_omitRecoveryAttempt` (`agent-session.ts`). A length/error stop ends
/// the run without executing tools, so the attempt is the transcript tail
/// with nothing following it. The transcript is only rewritten after the
/// `context_edit` tombstone reached the append-only log (the commit order of
/// [`apply_compaction`]): the omitted entry stays in the log for audit, the
/// tombstone keeps resumed sessions from replaying it, and the persist cursor
/// is rebased so the removed message is never written twice.
fn omit_failed_attempt(agent: &Agent, log: &std::sync::Mutex<SharedLog>) -> anyhow::Result<()> {
	let snapshot = agent.snapshot();
	let Some(Message::Assistant(_)) = snapshot.messages.last() else {
		anyhow::bail!("transcript does not end with the failed assistant");
	};
	// Verify the durable boundary under the log lock (released before the
	// transaction: its commit closure re-locks to append the tombstone).
	let target_id = {
		let guard = log.lock().unwrap();
		if guard.persisted != snapshot.messages.len() {
			anyhow::bail!("transcript is not fully persisted");
		}
		if guard.session.session_dir().is_none() {
			anyhow::bail!("overflow recovery needs a session log");
		}
		let projection = guard.session.projection();
		let Some((entry_id, logged)) = projection.messages.last() else {
			anyhow::bail!("session log is empty");
		};
		let Some(target_id) = entry_id.clone() else {
			anyhow::bail!("failed attempt has no log entry id");
		};
		if !message_projection_eq(logged, snapshot.messages.last().unwrap()) {
			anyhow::bail!("session log tail is not the failed attempt");
		}
		target_id
	};
	let mut without_attempt = snapshot.messages.clone();
	without_attempt.pop();
	let tombstone = SessionEntry::context_edit(target_id);
	agent.replace_messages_transactionally(&snapshot.messages, without_attempt, || {
		log.lock().unwrap().session.append(tombstone)?;
		Ok(())
	})?;
	// The failed attempt was persisted behind the old cursor; everything that
	// remains was persisted before it, so rebasing keeps the log append-only
	// without duplicating any of them.
	log.lock().unwrap().persisted = agent.snapshot().messages.len();
	Ok(())
}

/// Apply a mid-run compaction committed by the `prepare_next_turn` hook: the
/// log entry is already written, so this only swaps the transcript — now
/// legal because the run has ended — extending the kept tail with whatever
/// the loop appended after the hook (drained steering/follow-ups). The
/// expected-transcript check requires the hook-time transcript to be a
/// prefix of the current one; on any mismatch the log entry still wins on
/// resume (the projection reconciles), so this only reports and moves on.
fn apply_mid_run_compaction(
	agent: &Agent,
	log: &std::sync::Mutex<SharedLog>,
	bus: &EventBus<SessionEvent>,
) -> bool {
	let mut guard = log.lock().unwrap();
	let Some(pending) = guard.pending_mid_run.take() else {
		return true;
	};
	let outcome = pending.outcome;
	let current = agent.snapshot().messages;
	let source = &outcome.source_messages;
	let prefix_matches = current.len() >= source.len() && current[..source.len()] == source[..];
	if !prefix_matches {
		bus.emit(SessionEvent::Notice(
			"mid-run compaction skipped: the transcript changed after the summary; reopen to reconcile".into(),
		));
		return true;
	}
	let mut history = compaction::replacement_history(source, &outcome.summary);
	history.extend(outcome.kept.iter().cloned());
	history.extend(current[source.len()..].iter().cloned());
	let result = agent.replace_messages_transactionally(&current, history, || Ok(()));
	if let Err(error) = result {
		bus.emit(SessionEvent::Notice(format!(
			"compaction commit failed: {error}"
		)));
		return false;
	}
	// The summary reached the log through the compaction entry; rebase the
	// persist cursor onto the replaced transcript so nothing is double-written.
	guard.persisted = agent.snapshot().messages.len();
	bus.emit(SessionEvent::Notice(format!(
		"compacted: {} tokens summarized",
		outcome.tokens_before
	)));
	true
}

/// Commit a finished compaction: write the log entry first — it is the
/// authoritative record (upstream `SessionManager.appendCompaction`) — then
/// swap the agent history. A failed write leaves the transcript untouched, so
/// the log never claims a compaction the agent did not apply.
fn apply_compaction(
	agent: &Agent,
	log: &std::sync::Mutex<SharedLog>,
	bus: &EventBus<SessionEvent>,
	outcome: CompactionOutcome,
) -> bool {
	let mut entry = SessionEntry::compaction(outcome.summary.clone(), outcome.tokens_before);
	if let (
		Some(id),
		EntryKind::Compaction {
			first_kept_entry_id,
			..
		},
	) = (&outcome.first_kept_id, &mut entry.kind)
	{
		*first_kept_entry_id = id.clone();
	}
	let mut history = compaction::replacement_history(&outcome.source_messages, &outcome.summary);
	history.extend(outcome.kept);
	let result = agent.replace_messages_transactionally(&outcome.source_messages, history, || {
		log.lock().unwrap().session.append(entry)?;
		Ok(())
	});
	if let Err(error) = result {
		bus.emit(SessionEvent::Notice(format!(
			"compaction commit failed: {error}"
		)));
		return false;
	}
	if log.lock().unwrap().session.session_dir().is_some() {
		bus.emit(SessionEvent::Persisted { kind: "compaction" });
	}

	// The summary reached the log through the compaction entry, never as a
	// message entry: rebase the persist cursor onto the replaced transcript so
	// it is not double-persisted.
	log.lock().unwrap().persisted = agent.snapshot().messages.len();
	bus.emit(SessionEvent::Notice(format!(
		"compacted: {} tokens summarized",
		outcome.tokens_before
	)));
	true
}

fn next_thinking_level(current: ThinkingLevel) -> ThinkingLevel {
	use ThinkingLevel::*;
	match current {
		Off => Minimal,
		Minimal => Low,
		Low => Medium,
		Medium => High,
		High => Xhigh,
		Xhigh => Max,
		Max => Off,
	}
}

/// Rebuild a transcript from a session file (resume support) — the tree
/// projection from `session-manager.rs` (`buildContextEntries` +
/// `buildSessionProjection` upstream): the leaf-to-root path with the newest
/// compaction applied (its summary replays as a message, older summarized
/// entries omitted, the kept range from `firstKeptEntryId`) and the latest
/// `context_edit` per target honored.
pub fn read_session_messages(dir: &std::path::Path) -> Vec<Message> {
	session_manager::project_file(dir)
		.messages
		.into_iter()
		.map(|(_, message)| message)
		.collect()
}

/// Messages to seed a resumed agent (`--continue` / `--resume`): the persisted
/// transcript minus system messages. The persist cursor starts past the leading
/// system message, so the log never contains one; on resume [`Agent::new`]
/// (pi-agent) seeds a fresh system message from the current environment —
/// cwd, tools, freshly built prompt — mirroring the upstream
/// [`_rebuildSystemPrompt`] behaviour in `core/agent-session.ts`. A compaction
/// entry replays as an ordinary user message (see
/// [`read_session_messages`]), so it survives the filter and the fresh system
/// message lands ahead of it, matching the post-compaction transcript shape.
pub fn restore_initial_messages(dir: &std::path::Path) -> Vec<Message> {
	read_session_messages(dir)
		.into_iter()
		.filter(|message| !matches!(message, Message::System(_)))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_agent::AgentOptions;
	use serde_json::Value;
	use std::time::Duration;

	fn mock_agent() -> Agent {
		Agent::new(AgentOptions {
			stream_fn: Some(agent_stream_fn()),
			..Default::default()
		})
	}

	fn agent_stream_fn() -> pi_ai::StreamFn {
		Arc::new(
			|model: &pi_ai::Model,
			 context: &pi_ai::TranscriptContext,
			 options: pi_ai::StreamOptions| {
				pi_ai::providers::mock::stream(model, context, options)
			},
		)
	}

	#[test]
	fn overflow_patterns_match_provider_wording() {
		let model = pi_ai::Model::new("m", "m", "openai-completions", "m", "http://localhost");
		let mut message = echo_assistant_message(&model, String::new());
		message.stop_reason = StopReason::Error;
		for error in [
			// Anthropic
			"prompt is too long: 213462 tokens > 200000 maximum",
			// OpenAI
			"Your input exceeds the context window of this model",
			// Google Gemini
			"The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
			// OpenRouter/Poolside
			"Input length 265330 exceeds the maximum allowed input length of 262144.",
			// raw HTTP 413 status line and GitHub Copilot's SDK prefix
			"413 Request Entity Too Large",
			"github-copilot API error (413): {\"message\":\"failed to parse request\"}",
		] {
			message.error_message = Some(error.into());
			assert!(is_context_overflow(&message, 0), "`{error}` is overflow");
		}
		// Throttling and rate limiting are excluded even with overlapping
		// fragments ("too many tokens", "exceeded").
		message.error_message =
			Some("ThrottlingException: Too many tokens, please wait before trying again.".into());
		assert!(!is_context_overflow(&message, 0));
		message.error_message = Some("rate limit exceeded, retry later".into());
		assert!(!is_context_overflow(&message, 0));
		// Silent overflow: a successful stop whose input passed the window.
		message.stop_reason = StopReason::Stop;
		message.error_message = None;
		message.usage.input = 250_000;
		assert!(is_context_overflow(&message, 200_000));
		assert!(!is_context_overflow(&message, 0));
		// Length-stop overflow: zero output on a window-filling input.
		message.stop_reason = StopReason::Length;
		message.usage.input = 199_000;
		message.usage.output = 0;
		assert!(is_context_overflow(&message, 200_000));
		message.usage.output = 100;
		assert!(!is_context_overflow(&message, 200_000));
		// Recoverable length: truncated below the model's output limit.
		message.usage.output = 99;
		assert!(is_recoverable_length(&message, 100));
		assert!(!is_recoverable_length(&message, 0));
		message.usage.output = 100;
		assert!(!is_recoverable_length(&message, 100));
	}

	/// Scripted provider for recovery tests: summarization requests echo the
	/// conversation; ordinary request `turn` plays `script(turn, model)`.
	fn recovery_stream_fn(
		script: impl Fn(usize, &pi_ai::Model) -> pi_ai::AssistantMessageEvent + Send + Sync + 'static,
	) -> (pi_ai::StreamFn, std::sync::Arc<std::sync::Mutex<usize>>) {
		let script = std::sync::Arc::new(script);
		let requests = std::sync::Arc::new(std::sync::Mutex::new(0usize));
		let counter = requests.clone();
		let stream: pi_ai::StreamFn = Arc::new(move |model, context, _| {
			if is_summarization_request(context) {
				let model = model.clone();
				let text = format!("[mock summary] {}", last_user_text(context));
				return Box::pin(futures::stream::once(async move {
					pi_ai::AssistantMessageEvent::Done {
						reason: StopReason::Stop,
						message: echo_assistant_message(&model, text),
					}
				})) as pi_ai::AssistantMessageEventStream;
			}
			let turn = {
				let mut count = counter.lock().unwrap();
				*count += 1;
				*count
			};
			let script = script.clone();
			let model = model.clone();
			Box::pin(futures::stream::once(async move { script(turn, &model) }))
				as pi_ai::AssistantMessageEventStream
		});
		(stream, requests)
	}

	fn stop_reply(model: &pi_ai::Model, text: &str) -> pi_ai::AssistantMessageEvent {
		pi_ai::AssistantMessageEvent::Done {
			reason: StopReason::Stop,
			message: echo_assistant_message(model, text.to_string()),
		}
	}

	fn overflow_error(model: &pi_ai::Model) -> pi_ai::AssistantMessageEvent {
		let mut message = echo_assistant_message(model, String::new());
		message.stop_reason = StopReason::Error;
		message.error_message = Some("prompt is too long: 213462 tokens > 200000 maximum".into());
		pi_ai::AssistantMessageEvent::Error {
			reason: StopReason::Error,
			error: message,
		}
	}

	fn truncated_length(model: &pi_ai::Model, output: u64) -> pi_ai::AssistantMessageEvent {
		let mut message = echo_assistant_message(model, "partial answer".into());
		message.stop_reason = StopReason::Length;
		message.usage.output = output;
		pi_ai::AssistantMessageEvent::Done {
			reason: StopReason::Length,
			message,
		}
	}

	fn recovery_model() -> pi_ai::Model {
		let mut model = pi_ai::Model::new(
			"mock",
			"mock",
			"openai-completions",
			"mock",
			"http://localhost",
		);
		model.context_window = 30_000;
		model.max_tokens = 100;
		model
	}

	fn recovery_settings() -> CompactionSettings {
		CompactionSettings {
			enabled: Some(true),
			reserve_tokens: Some(16_384),
			// keep-recent 0: the tiny scripted transcripts stay below any real
			// budget, so only a zero budget yields a summarizable prefix while
			// keeping the newest turn (the cut lands on its user message).
			keep_recent_tokens: Some(0),
		}
	}

	/// Mid-run compaction (the `prepare_next_turn` between-turn check): turn
	/// 1 reports usage past the threshold while a steering message keeps the
	/// run going; the hook compacts before the next request. The mock
	/// records every ordinary request's role sequence, so the test can
	/// assert the second request already carries the compacted context.
	#[tokio::test]
	async fn mid_run_threshold_compacts_before_the_next_request() {
		let dir = temp_session_dir("mid-run");
		let request_roles = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
		let ordinary_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
		let roles = request_roles.clone();
		let stream: pi_ai::StreamFn = Arc::new(move |model, context, _| {
			let roles = roles.clone();
			if is_summarization_request(context) {
				let model = model.clone();
				let text = format!("[mock summary] {}", last_user_text(context));
				return Box::pin(futures::stream::once(async move {
					pi_ai::AssistantMessageEvent::Done {
						reason: StopReason::Stop,
						message: echo_assistant_message(&model, text),
					}
				})) as pi_ai::AssistantMessageEventStream;
			}
			roles.lock().unwrap().push(
				context
					.messages
					.iter()
					.map(|message| {
						let role = message.role().to_string();
						match message {
							Message::User(user) if user.content.as_text().contains("summary") => {
								format!("{role}:summary")
							}
							Message::User(user) => {
								format!("{role}:{}", user.content.as_text())
							}
							_ => role,
						}
					})
					.collect(),
			);
			// Only turn 1 crosses the threshold (window 30_000, reserve
			// 16_384); later replies report no usage so exactly one mid-run
			// compaction happens.
			let first = ordinary_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
			let model = model.clone();
			Box::pin(futures::stream::once(async move {
				let mut reply = echo_assistant_message(&model, "answer".into());
				if first {
					reply.usage.total_tokens = 20_000;
				}
				pi_ai::AssistantMessageEvent::Done {
					reason: StopReason::Stop,
					message: reply,
				}
			})) as pi_ai::AssistantMessageEventStream
		});
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session_with_settings(
			agent.clone(),
			SessionManager::open(&dir).unwrap(),
			registry,
			recovery_settings(),
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "first".into(),
			})
			.unwrap();
		// Steer while the run is active so the loop continues after turn 1
		// and the between-turn hook fires before the second request.
		handle
			.cmd_tx
			.send(SessionCommand::Steer {
				text: "second".into(),
			})
			.unwrap();
		let notices = notices_until_agent_ends(&mut handle, 2).await;
		assert!(
			notices
				.iter()
				.any(|text| text == "auto-compaction (mid-run)…"),
			"hook notice missing: {notices:?}"
		);
		assert!(notices.iter().any(|text| text.starts_with("compacted:")));

		// Two ordinary requests: the second already carries the compaction
		// summary (upstream compacts during prepareNextTurn, before the next
		// assistant response).
		// Clone out of the guard: asserts below await.
		let requests = request_roles.lock().unwrap().clone();
		assert_eq!(requests.len(), 2, "two ordinary requests");
		assert_eq!(requests[0], vec!["system", "user:first"]);
		assert_eq!(
			requests[1],
			vec![
				"system",
				"user:summary", // the compaction summary message
				"assistant",    // turn 1's reply (kept tail)
				"user:second",  // the steering message
			]
		);

		// The transcript was swapped to summary + kept + steering.
		let roles: Vec<&str> = agent
			.snapshot()
			.messages
			.iter()
			.map(Message::role)
			.collect();
		assert_eq!(roles, ["system", "user", "assistant", "user", "assistant"]);
		let snapshot = agent.snapshot();
		let Message::User(summary) = &snapshot.messages[1] else {
			panic!("expected the compaction summary")
		};
		let summary_text = summary.content.as_text();
		assert!(summary_text.contains(compaction::COMPACTION_SUMMARY_PREFIX));
		assert!(summary_text.contains("[mock summary]"));
		assert!(summary_text.contains("first"));

		// Log: exactly one compaction entry, and the log replays to the live
		// transcript (the entry led the swap; steering appended after it).
		assert_eq!(compaction_entry_values(&dir).len(), 1);
		wait_for_log_len(&dir, snapshot.messages.len() - 1).await;
		let restored = read_session_messages(&dir);
		let restored_roles: Vec<&str> = restored.iter().map(Message::role).collect();
		assert_eq!(restored_roles, ["user", "assistant", "user", "assistant"]);
		assert!(
			restored
				.iter()
				.zip(agent.snapshot().messages.iter().skip(1))
				.all(|(a, b)| message_projection_eq(a, b))
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Wait until the log holds at least `count` projected messages (the task
	/// persists concurrently with the run).
	async fn wait_for_log_len(dir: &std::path::Path, count: usize) {
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(dir).len() < count {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("log should reach the expected length");
	}

	/// A single oversized turn is compacted (split turn, upstream
	/// `findProjectedCutPoint`): the cut lands on the turn's assistant reply,
	/// the turn's head gets the turn-prefix summary, and the merged summary
	/// lands in the log behind the upstream split-turn label.
	#[tokio::test]
	async fn oversized_single_turn_is_split_and_compacted() {
		let dir = temp_session_dir("split-turn");
		// Instant echo provider: the raw mock streams one delta per 24 chars
		// with a 12ms pause each, which no test run can afford at 85k chars.
		let stream: pi_ai::StreamFn = Arc::new(|model, context, _| {
			let model = model.clone();
			let text = if is_summarization_request(context) {
				format!("[mock summary] {}", last_user_text(context))
			} else {
				"reply".to_string()
			};
			Box::pin(futures::stream::once(async move {
				pi_ai::AssistantMessageEvent::Done {
					reason: StopReason::Stop,
					message: echo_assistant_message(&model, text),
				}
			})) as pi_ai::AssistantMessageEventStream
		});
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let session = SessionManager::open(&dir).unwrap();
		let mut handle = spawn_agent_session_with_settings(
			agent.clone(),
			session,
			registry,
			recovery_settings(),
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "x".repeat(85_000),
			})
			.unwrap();
		notices_until_agent_ends(&mut handle, 1).await;

		let entries = compaction_entry_values(&dir);
		assert_eq!(entries.len(), 1, "one split-turn compaction");
		let summary = entries[0]["summary"].as_str().unwrap();
		assert!(
			summary.contains("\n\n---\n\n**Turn Context (split turn):**\n\n"),
			"merged split-turn summary: {summary}"
		);
		assert!(summary.starts_with("No prior history."));
		assert!(summary.contains("[mock summary]"), "{summary}");
		// The kept tail starts at the turn's assistant reply (the entry is
		// not the compaction itself), and the projection replays
		// summary + kept reply.
		assert_ne!(entries[0]["firstKeptEntryId"], entries[0]["id"]);
		wait_for_log_len(&dir, 2).await;
		let restored = read_session_messages(&dir);
		assert_eq!(restored.len(), 2);
		assert!(
			matches!(&restored[0], Message::User(user) if user.content.as_text().contains("summary"))
		);
		assert!(matches!(restored[1], Message::Assistant(_)));
		// The live transcript was swapped to [system, summary, kept reply].
		let roles: Vec<&str> = agent
			.snapshot()
			.messages
			.iter()
			.map(Message::role)
			.collect();
		assert_eq!(roles, ["system", "user", "assistant"]);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Collects every notice emitted while `ends` more runs finish. Notices
	/// preceding a buffered `AgentEnd` (the recovery trigger fires in the
	/// run-completion branch, ahead of the re-emitted agent events) are part
	/// of the window and must not be dropped.
	async fn notices_until_agent_ends(handle: &mut SessionHandle, ends: usize) -> Vec<String> {
		let mut notices = Vec::new();
		tokio::time::timeout(Duration::from_secs(30), async {
			let mut completed = 0;
			while completed < ends {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text)) => notices.push(text),
					Ok(SessionEvent::Agent(event))
						if matches!(*event, AgentEvent::AgentEnd { .. }) =>
					{
						completed += 1;
					}
					Ok(_) => {}
					Err(e) => panic!("event stream error: {e}"),
				}
			}
		})
		.await
		.expect("runs should finish in time");
		notices
	}

	fn context_edit_count(dir: &std::path::Path) -> usize {
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		log.lines()
			.filter_map(|line| serde_json::from_str::<Value>(line).ok())
			.filter(|value| value["type"] == "context_edit")
			.count()
	}

	/// A run ending in a context-overflow error recovers once: the failed
	/// attempt is omitted (tombstoned in the append-only log), the older prefix
	/// is compacted, and the turn is retried exactly once.
	#[tokio::test]
	async fn overflow_error_recovers_through_compaction_and_retries_once() {
		let dir = temp_session_dir("overflow-error");
		let (stream, requests) = recovery_stream_fn(|turn, model| match turn {
			1 => stop_reply(model, "first answer"),
			2 => overflow_error(model),
			_ => stop_reply(model, "recovered answer"),
		});
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session_with_settings(
			agent.clone(),
			SessionManager::open(&dir).unwrap(),
			registry,
			recovery_settings(),
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "first".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "second".into(),
			})
			.unwrap();
		// Turn 2 ends with the overflow; the recovery compaction and the retry
		// run before the next agent_end.
		let notices = notices_until_agent_ends(&mut handle, 2).await;
		assert_eq!(*requests.lock().unwrap(), 3, "retried exactly once");
		assert!(
			notices
				.iter()
				.any(|t| t == "context overflow: compacting before one retry")
		);
		assert!(notices.iter().any(|t| t.starts_with("compacted:")));
		assert_eq!(
			notices
				.iter()
				.filter(|t| t.contains("context overflow"))
				.count(),
			1,
			"only one recovery attempt"
		);

		// Transcript: the failed attempt is gone, the kept prompt and the
		// retried reply follow the summary.
		let snapshot = agent.snapshot();
		let roles: Vec<&str> = snapshot.messages.iter().map(Message::role).collect();
		assert_eq!(roles, ["system", "user", "user", "assistant"]);
		assert!(
			!snapshot
				.messages
				.iter()
				.any(|m| matches!(m, Message::Assistant(a) if a.stop_reason == StopReason::Error))
		);
		let Message::Assistant(last) = snapshot.messages.last().unwrap() else {
			panic!("expected the retried reply")
		};
		assert_eq!(last.text(), "recovered answer");

		// Log: one compaction, one tombstone, the failed attempt written
		// exactly once — never re-persisted after the omission.
		assert_eq!(compaction_entry_values(&dir).len(), 1);
		assert_eq!(context_edit_count(&dir), 1);
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(
			log.matches("prompt is too long").count(),
			1,
			"the failed attempt must not be persisted twice"
		);

		// Replay: summary + kept prompt + retry reply, still error-free.
		let restored = read_session_messages(&dir);
		let roles: Vec<&str> = restored.iter().map(Message::role).collect();
		assert_eq!(roles, ["user", "user", "assistant"]);
		assert!(
			restored
				.iter()
				.all(|m| !matches!(m, Message::Assistant(a) if a.stop_reason == StopReason::Error))
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A length truncation below the model's output limit recovers the same
	/// way as an overflow error.
	#[tokio::test]
	async fn recoverable_length_recovers_through_compaction_and_retries_once() {
		let dir = temp_session_dir("overflow-length");
		let (stream, requests) = recovery_stream_fn(|turn, model| match turn {
			1 => stop_reply(model, "first answer"),
			2 => truncated_length(model, 5),
			_ => stop_reply(model, "recovered answer"),
		});
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session_with_settings(
			agent.clone(),
			SessionManager::open(&dir).unwrap(),
			registry,
			recovery_settings(),
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "first".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "second".into(),
			})
			.unwrap();
		// Turn 2 ends with the overflow; the recovery compaction and the retry
		// run before the next agent_end.
		let notices = notices_until_agent_ends(&mut handle, 2).await;
		assert_eq!(*requests.lock().unwrap(), 3, "retried exactly once");
		assert!(
			notices
				.iter()
				.any(|t| t == "context overflow: compacting before one retry")
		);
		assert_eq!(compaction_entry_values(&dir).len(), 1);
		let snapshot = agent.snapshot();
		let roles: Vec<&str> = snapshot.messages.iter().map(Message::role).collect();
		assert_eq!(roles, ["system", "user", "user", "assistant"]);
		assert!(
			!snapshot
				.messages
				.iter()
				.any(|m| matches!(m, Message::Assistant(a) if a.stop_reason == StopReason::Length))
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A second overflow after the recovery retry gives up instead of looping:
	/// the retried attempt stays in the transcript and the log gains no second
	/// compaction or tombstone.
	#[tokio::test]
	async fn second_overflow_gives_up_after_one_retry() {
		let dir = temp_session_dir("overflow-giveup");
		let (stream, requests) = recovery_stream_fn(|turn, model| match turn {
			1 => stop_reply(model, "first answer"),
			_ => overflow_error(model),
		});
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session_with_settings(
			agent.clone(),
			SessionManager::open(&dir).unwrap(),
			registry,
			recovery_settings(),
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "first".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "second".into(),
			})
			.unwrap();
		// Turn 2 ends with the overflow; the recovery compaction and the retry
		// run before the next agent_end.
		let notices = notices_until_agent_ends(&mut handle, 2).await;
		assert_eq!(*requests.lock().unwrap(), 3);
		assert!(
			notices
				.iter()
				.any(|t| t.starts_with("overflow recovery failed after one compact-and-retry"))
		);
		assert_eq!(compaction_entry_values(&dir).len(), 1);
		assert_eq!(context_edit_count(&dir), 1);
		// The retried attempt stays: no second omission after a failed retry.
		let snapshot = agent.snapshot();
		let Message::Assistant(last) = snapshot.messages.last().unwrap() else {
			panic!("expected the failed retry")
		};
		assert_eq!(last.stop_reason, StopReason::Error);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn recent_tail_compaction_replays_identically() {
		let dir = temp_session_dir("compact-tail");
		let mut session = SessionManager::open(&dir).unwrap();
		let messages = vec![
			Message::user("old turn"),
			Message::user("recent ".repeat(15000)),
		];
		for message in &messages {
			session.append_message(message).unwrap();
		}
		let agent = Agent::new(AgentOptions {
			initial_messages: messages,
			stream_fn: Some(compaction_test_stream_fn()),
			..Default::default()
		});
		let log = std::sync::Mutex::new(SharedLog {
			session,
			persisted: agent.snapshot().messages.len(),
			pending_mid_run: None,
			generation: 0,
			pending_bash: Vec::new(),
		});
		let cut = {
			let mut guard = log.lock().unwrap();
			auto_compaction_cut(&agent, &mut guard, 20_000).expect("safe boundary")
		};
		assert_eq!(cut.first_kept, 1);
		let outcome = compaction::compact_prefix(&agent, Some(cut)).await.unwrap();
		let bus = EventBus::new();
		apply_compaction(&agent, &log, &bus, outcome);
		let actual: Vec<_> = agent
			.snapshot()
			.messages
			.into_iter()
			.filter(|message| !matches!(message, Message::System(_)))
			.collect();
		let restored = read_session_messages(&dir);
		assert_eq!(actual.len(), restored.len());
		assert!(
			actual
				.iter()
				.zip(&restored)
				.all(|(a, b)| message_projection_eq(a, b))
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn threshold_compacts_and_preserves_recent_turn_on_resume() {
		let dir = temp_session_dir("auto-compact");
		let mut model = pi_ai::Model::new(
			"mock",
			"mock",
			"openai-completions",
			"mock",
			"http://localhost",
		);
		model.context_window = 30_000;
		let agent = Agent::new(AgentOptions {
			model,
			stream_fn: Some(Arc::new(|model, context, _| {
				let model = model.clone();
				let text = if is_summarization_request(context) {
					"summary"
				} else {
					"answer"
				}
				.to_string();
				Box::pin(futures::stream::once(async move {
					pi_ai::AssistantMessageEvent::Done {
						reason: pi_ai::StopReason::Stop,
						message: echo_assistant_message(&model, text),
					}
				}))
			})),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt { text: "old".into() })
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "x".repeat(85_000),
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				if let Ok(SessionEvent::Notice(text)) = handle.events.recv().await
					&& text.starts_with("compacted:")
				{
					break;
				}
			}
		})
		.await
		.expect("automatic compaction should finish");
		let restored = read_session_messages(&dir);
		let current: Vec<_> = agent
			.snapshot()
			.messages
			.into_iter()
			.filter(|m| !matches!(m, Message::System(_)))
			.collect();
		assert_eq!(current.len(), restored.len());
		assert!(
			current
				.iter()
				.zip(&restored)
				.all(|(a, b)| message_projection_eq(a, b))
		);
		assert!(
			restored.iter().any(
				|m| matches!(m, Message::User(user) if user.content.as_text().len() == 85_000)
			)
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn failed_compaction_write_keeps_transcript() {
		let dir = temp_session_dir("compact-write-fail");
		let path = dir.join("readonly.jsonl");
		std::fs::write(&path, "").unwrap();
		let session = SessionManager::with_file(dir.clone(), std::fs::File::open(path).unwrap());
		let agent = Agent::new(AgentOptions {
			initial_messages: vec![Message::user("original")],
			..Default::default()
		});
		let before = agent.snapshot().messages;
		let log = std::sync::Mutex::new(SharedLog {
			session,
			persisted: before.len(),
			pending_mid_run: None,
			generation: 0,
			pending_bash: Vec::new(),
		});
		apply_compaction(
			&agent,
			&log,
			&EventBus::new(),
			CompactionOutcome {
				summary: "summary".into(),
				tokens_before: 1,
				kept: vec![],
				first_kept_id: None,
				source_messages: before.clone(),
			},
		);
		let after = agent.snapshot().messages;
		assert_eq!(before.len(), after.len());
		assert!(
			before
				.iter()
				.zip(&after)
				.all(|(a, b)| message_projection_eq(a, b))
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn manual_compaction_refuses_unpersisted_turn() {
		let dir = temp_session_dir("compact-unpersisted");
		let path = dir.join("readonly.jsonl");
		std::fs::write(&path, "").unwrap();
		let session = SessionManager::with_file(dir.clone(), std::fs::File::open(path).unwrap());
		let agent = Agent::new(AgentOptions {
			stream_fn: Some(compaction_test_stream_fn()),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent.clone(), session, registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "original".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle.cmd_tx.send(SessionCommand::Compact).unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				if let Ok(SessionEvent::Notice(text)) = handle.events.recv().await
					&& text == "cannot compact: transcript is not fully persisted"
				{
					break;
				}
			}
		})
		.await
		.unwrap();
		assert!(
			agent
				.snapshot()
				.messages
				.iter()
				.any(|m| matches!(m, Message::User(user) if user.content.as_text() == "original"))
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn aborting_compaction_starts_queued_prompt() {
		let stream: pi_ai::StreamFn = Arc::new(|model, context, _| {
			if is_summarization_request(context) {
				Box::pin(futures::stream::pending()) as pi_ai::AssistantMessageEventStream
			} else {
				let model = model.clone();
				Box::pin(futures::stream::once(async move {
					pi_ai::AssistantMessageEvent::Done {
						reason: pi_ai::StopReason::Stop,
						message: echo_assistant_message(&model, "answer".into()),
					}
				}))
			}
		});
		let agent = Agent::new(AgentOptions {
			initial_messages: vec![Message::user("previous")],
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent.clone(), SessionManager::in_memory(), registry);
		handle.cmd_tx.send(SessionCommand::Compact).unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				if let Ok(SessionEvent::Notice(text)) = handle.events.recv().await
					&& text == "compacting…"
				{
					break;
				}
			}
		})
		.await
		.unwrap();
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "queued".into(),
			})
			.unwrap();
		handle.cmd_tx.send(SessionCommand::Abort).unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		assert!(
			agent
				.snapshot()
				.messages
				.iter()
				.any(|m| matches!(m, Message::User(user) if user.content.as_text() == "queued"))
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	#[tokio::test]
	async fn sync_failure_stops_session_before_queued_prompt() {
		let dir = temp_session_dir("compact-sync-stop");
		let mut session = SessionManager::open(&dir).unwrap();
		session.append_message(&Message::user("original")).unwrap();
		let agent = Agent::new(AgentOptions {
			initial_messages: restore_initial_messages(&dir),
			stream_fn: Some(compaction_test_stream_fn()),
			..Default::default()
		});
		session.fail_next_sync();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent.clone(), session, registry);
		handle.cmd_tx.send(SessionCommand::Compact).unwrap();
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "must not run".into(),
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text))
						if text.contains("session stopped after compaction commit failure") =>
					{
						break;
					}
					Ok(_) => {}
					Err(e) => panic!("missing stop notice: {e}"),
				}
			}
		})
		.await
		.unwrap();
		assert!(
			!agent.snapshot().messages.iter().any(
				|m| matches!(m, Message::User(user) if user.content.as_text() == "must not run")
			)
		);
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert!(!log.contains("must not run"));
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn sync_failure_does_not_apply_compaction_to_live_transcript() {
		let dir = temp_session_dir("compact-sync-fail");
		let mut session = SessionManager::open(&dir).unwrap();
		let original = Message::user("original");
		session.append_message(&original).unwrap();
		let agent = Agent::new(AgentOptions {
			initial_messages: vec![original],
			..Default::default()
		});
		let before = agent.snapshot().messages;
		let outcome = CompactionOutcome {
			summary: "summary".into(),
			tokens_before: 1,
			kept: vec![],
			first_kept_id: None,
			source_messages: before.clone(),
		};
		let persisted = before.len();
		session.fail_next_sync();
		let log = std::sync::Mutex::new(SharedLog {
			session,
			persisted,
			pending_mid_run: None,
			generation: 0,
			pending_bash: Vec::new(),
		});
		assert!(!apply_compaction(&agent, &log, &EventBus::new(), outcome));
		assert_eq!(agent.snapshot().messages, before);
		assert_eq!(log.lock().unwrap().persisted, before.len());
		// A written but unsynced compaction may be recovered on reopen; the
		// session task must stop rather than append a divergent continuation.
		assert_eq!(read_session_messages(&dir).len(), 1);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn restored_usage_boundary_excludes_kept_assistant_usage() {
		let dir = temp_session_dir("usage-boundary");
		let mut session = SessionManager::open(&dir).unwrap();
		let model = pi_ai::Model::new(
			"mock",
			"mock",
			"openai-completions",
			"mock",
			"http://localhost",
		);
		let mut prior = echo_assistant_message(&model, "old answer".into());
		prior.usage.total_tokens = 100;
		session.append_message(&Message::user("old")).unwrap();
		session.append_message(&Message::user("keep")).unwrap();
		session.append_message(&Message::Assistant(prior)).unwrap();
		let projection = session_manager::project_file(&dir);
		let mut entry = SessionEntry::compaction("summary", 100);
		if let EntryKind::Compaction {
			first_kept_entry_id,
			..
		} = &mut entry.kind
		{
			*first_kept_entry_id = projection.messages[1].0.clone().unwrap();
		}
		session.append(entry).unwrap();
		let agent = Agent::new(AgentOptions {
			initial_messages: restore_initial_messages(&dir),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let handle = spawn_agent_session(agent.clone(), session, registry);
		assert_eq!(
			agent.snapshot().usage_valid_from,
			agent.snapshot().messages.len()
		);
		assert!(super::super::context_usage::compute(&agent).is_none());
		drop(handle);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn dangling_first_kept_entry_drops_older_entries() {
		// Upstream `buildContextEntries` semantics: the compaction entry
		// claims everything before `firstKeptEntryId`; a dangling id retains
		// nothing (the raw entries stay on disk — the log is append-only).
		let dir = temp_session_dir("missing-cut");
		let mut session = SessionManager::open(&dir).unwrap();
		session
			.append_message(&Message::user("safe history"))
			.unwrap();
		let mut entry = SessionEntry::compaction("summary", 1);
		if let EntryKind::Compaction {
			first_kept_entry_id,
			..
		} = &mut entry.kind
		{
			*first_kept_entry_id = "missing".into();
		}
		session.append(entry).unwrap();
		let restored = read_session_messages(&dir);
		assert_eq!(restored.len(), 1);
		let Message::User(summary) = &restored[0] else {
			panic!("expected the summary message")
		};
		assert!(summary.content.as_text().contains("summary"));
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn rapid_prompts_are_both_processed() {
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent.clone(), SessionManager::in_memory(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "first".into(),
			})
			.unwrap();
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "second".into(),
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			let mut completed = 0;
			while completed < 2 {
				if let Ok(SessionEvent::Agent(event)) = handle.events.recv().await
					&& matches!(*event, AgentEvent::AgentEnd { .. })
				{
					completed += 1;
				}
			}
		})
		.await
		.expect("both prompts should complete");
		let messages = agent.snapshot().messages;
		let users: Vec<_> = messages
			.iter()
			.filter_map(|m| match m {
				Message::User(u) => Some(u.content.as_text()),
				_ => None,
			})
			.collect();
		assert_eq!(users, vec!["first", "second"]);
	}

	#[tokio::test]
	async fn persistence_reconciles_transcript_without_events() {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-session-test-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		let session = SessionManager::open(&dir).unwrap();
		let agent = mock_agent();
		let mut log = SharedLog {
			session,
			persisted: agent.snapshot().messages.len(),
			pending_mid_run: None,
			generation: 0,
			pending_bash: Vec::new(),
		};
		agent
			.prompt(PromptInput::Text {
				text: "hello".into(),
			})
			.await
			.unwrap();
		let bus = EventBus::new();
		persist_messages(&agent, &mut log, &bus);
		persist_messages(&agent, &mut log, &bus);
		let messages = read_session_messages(&dir);
		assert_eq!(messages.len(), 2);
		assert_eq!(messages[0].role(), "user");
		assert_eq!(messages[1].role(), "assistant");
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn exit_cancels_active_run_before_session_closes() {
		let dir = temp_session_dir("exit-active");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "hello".into(),
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			while !agent.is_streaming() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			while handle.events.recv().await.is_ok() {}
		})
		.await
		.expect("session must close");
		assert!(!agent.is_streaming());
		assert_eq!(
			read_session_messages(&dir).len(),
			agent.snapshot().messages.len() - 1
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn closing_command_channel_waits_for_active_run_and_persists_tail() {
		let dir = temp_session_dir("channel-close");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let (tx, rx) = mpsc::unbounded_channel();
		let bus = EventBus::new();
		let task = tokio::spawn(session_task(
			agent.clone(),
			Arc::new(std::sync::Mutex::new(SharedLog {
				session: SessionManager::open(&dir).unwrap(),
				persisted: 1,
				pending_mid_run: None,
				generation: 0,
				pending_bash: Vec::new(),
			})),
			registry,
			rx,
			bus,
			Arc::new(std::sync::atomic::AtomicBool::new(true)),
			Arc::new(std::sync::atomic::AtomicBool::new(false)),
			CompactionSettings::default(),
			Arc::new(SessionShared {
				live_agent: Arc::new(std::sync::Mutex::new(agent.clone())),
				current_session_file: Arc::new(std::sync::Mutex::new(Some(
					dir.join("session.jsonl"),
				))),
				session_name: Arc::new(std::sync::Mutex::new(None)),
				wire_events: tokio::sync::broadcast::channel(16).0,
				agent_end_will_retry: Arc::new(std::sync::Mutex::new(VecDeque::new())),
				retry: Arc::new(std::sync::Mutex::new(RetryRuntime {
					attempt: 0,
					base_delay_ms: retry_policy::RETRY_BASE_DELAY_MS,
				})),
			}),
			Arc::new(std::sync::atomic::AtomicBool::new(
				retry_policy::RETRY_ENABLED_DEFAULT,
			)),
		));
		tx.send(SessionCommand::Prompt {
			text: "hello".into(),
		})
		.unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			while !agent.is_streaming() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		drop(tx);
		tokio::time::timeout(Duration::from_secs(5), task)
			.await
			.unwrap()
			.unwrap();
		assert!(!agent.is_streaming());
		let messages = read_session_messages(&dir);
		assert_eq!(messages.len(), agent.snapshot().messages.len() - 1);
		assert_eq!(messages[0].role(), "user");
		std::fs::remove_dir_all(dir).unwrap();
	}

	fn temp_session_dir(tag: &str) -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-agent-session-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	// -- compaction -----------------------------------------------------------

	fn compaction_entry_values(dir: &std::path::Path) -> Vec<Value> {
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		log.lines()
			.filter_map(|line| serde_json::from_str::<Value>(line).ok())
			.filter(|value| value["type"] == "compaction")
			.collect()
	}

	/// A stop-level assistant message carrying `text` (echo replies).
	fn echo_assistant_message(model: &pi_ai::Model, text: String) -> pi_ai::AssistantMessage {
		pi_ai::AssistantMessage {
			content: vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
				text,
				text_signature: None,
			})],
			api: model.api.as_str().to_string(),
			provider: model.provider.clone(),
			model: model.id.clone(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: pi_ai::Usage::default(),
			stop_reason: pi_ai::StopReason::Stop,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: pi_ai::types::now_ms(),
			duration_ms: None,
		}
	}

	/// The last user message's text — the summarization request carries the
	/// whole conversation in one user message.
	fn last_user_text(context: &pi_ai::TranscriptContext) -> String {
		context
			.messages
			.iter()
			.rev()
			.find_map(|message| match message {
				Message::User(user) => Some(user.content.as_text()),
				_ => None,
			})
			.unwrap_or_default()
	}

	fn is_summarization_request(context: &pi_ai::TranscriptContext) -> bool {
		matches!(
			context.messages.first(),
			Some(Message::System(system))
				if matches!(
					&system.content,
					pi_ai::SystemContent::Text(text)
						if text.as_str() == compaction::SUMMARIZATION_SYSTEM_PROMPT
				)
		)
	}

	/// Mock-provider turns, except the standalone summarization call, which is
	/// answered with the mock's echo behavior. Upstream's verbatim
	/// `SUMMARIZATION_PROMPT` contains the word "list" ("Ordered list of what
	/// should happen next"), which trips `providers/mock.rs` into its
	/// tool-call path; the summarization request is recognized by its system
	/// prompt and echoed instead, so the summary carries the conversation back
	/// to the assertions.
	fn compaction_test_stream_fn() -> pi_ai::StreamFn {
		Arc::new(
			move |model: &pi_ai::Model,
			      context: &pi_ai::TranscriptContext,
			      options: pi_ai::StreamOptions| {
				if is_summarization_request(context) {
					let model = model.clone();
					let text = format!("[mock summary] {}", last_user_text(context));
					Box::pin(futures::stream::once(async move {
						pi_ai::AssistantMessageEvent::Done {
							reason: pi_ai::StopReason::Stop,
							message: echo_assistant_message(&model, text),
						}
					})) as pi_ai::AssistantMessageEventStream
				} else {
					pi_ai::providers::mock::stream(model, context, options)
				}
			},
		)
	}

	/// /compact round trip: the agent history shrinks to
	/// [system, summary], the log gains one parseable compaction entry, a
	/// follow-up turn appends after the summary, and a resumed session
	/// rebuilds the summary as an ordinary message without re-persisting old
	/// content. The mock provider acts as the summarizer (echo), so the
	/// summary text proves the transcript reached a standalone LLM call.
	#[tokio::test]
	async fn compact_replaces_history_logs_entry_and_survives_resume() {
		let dir = temp_session_dir("compact-roundtrip");
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));

		// First run: one turn, compact, one more turn.
		{
			let agent = Agent::new(AgentOptions {
				stream_fn: Some(compaction_test_stream_fn()),
				..Default::default()
			});
			let mut handle = spawn_agent_session(
				agent.clone(),
				SessionManager::open(&dir).unwrap(),
				registry.clone(),
			);
			handle
				.cmd_tx
				.send(SessionCommand::Prompt {
					text: "first question".into(),
				})
				.unwrap();
			wait_for_agent_ends(&mut handle, 1).await;
			assert_eq!(agent.snapshot().messages.len(), 3);

			handle.cmd_tx.send(SessionCommand::Compact).unwrap();
			tokio::time::timeout(Duration::from_secs(20), async {
				loop {
					match handle.events.recv().await {
						Ok(SessionEvent::Notice(text)) if text.starts_with("compacted:") => break,
						Ok(SessionEvent::Notice(text))
							if text.contains("compaction") && text != "compacting…" =>
						{
							panic!("compaction failed: {text}")
						}
						Ok(_) => {}
						Err(e) => panic!("event stream error: {e}"),
					}
				}
			})
			.await
			.expect("compaction should finish");

			// History replaced: system + the compaction summary message.
			let messages = agent.snapshot().messages;
			assert_eq!(messages.len(), 2);
			assert_eq!(messages[0].role(), "system");
			let Message::User(summary) = &messages[1] else {
				panic!("expected summary message")
			};
			let summary_text = summary.content.as_text();
			assert!(summary_text.contains(compaction::COMPACTION_SUMMARY_PREFIX));
			assert!(summary_text.contains(compaction::COMPACTION_SUMMARY_SUFFIX));
			// "[mock summary]" proves the transcript reached the standalone
			// summarization call; "[User]: first question" proves the
			// serialized conversation rode inside it.
			assert!(summary_text.contains("[mock summary]"));
			assert!(summary_text.contains("[User]: first question"));

			// The log carries exactly one parseable compaction entry.
			let entries = compaction_entry_values(&dir);
			assert_eq!(entries.len(), 1);
			assert!(
				entries[0]["summary"]
					.as_str()
					.unwrap()
					.contains("[mock summary]")
			);
			assert!(entries[0]["tokensBefore"].as_u64().unwrap() > 0);
			assert_eq!(entries[0]["firstKeptEntryId"], entries[0]["id"]);
			// Replay: the summarized turns collapse into the summary message.
			assert_eq!(read_session_messages(&dir).len(), 1);

			// A turn after compaction appends behind the summary.
			handle
				.cmd_tx
				.send(SessionCommand::Prompt {
					text: "second question".into(),
				})
				.unwrap();
			wait_for_agent_ends(&mut handle, 1).await;
			let roles: Vec<&str> = agent
				.snapshot()
				.messages
				.iter()
				.map(Message::role)
				.collect();
			assert_eq!(roles, ["system", "user", "user", "assistant"]);
			handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		}

		// Resume: the compaction entry rebuilds as an ordinary user message.
		// Wait for the second turn's tail to reach the log (the session task
		// persists concurrently with the run).
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(&dir).len() < 3 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("post-compaction turn should persist");
		let restored = restore_initial_messages(&dir);
		let roles: Vec<&str> = restored.iter().map(Message::role).collect();
		assert_eq!(roles, ["user", "user", "assistant"]);
		let Message::User(summary) = &restored[0] else {
			panic!("expected rebuilt summary message")
		};
		assert!(
			summary
				.content
				.as_text()
				.contains(compaction::COMPACTION_SUMMARY_PREFIX)
		);

		let resumed = Agent::new(AgentOptions {
			system_prompt: Some("resumed run".into()),
			stream_fn: Some(agent_stream_fn()),
			initial_messages: restored,
			..AgentOptions::default()
		});
		assert_eq!(resumed.snapshot().messages.len(), 4);
		let mut handle =
			spawn_agent_session(resumed, SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "third question".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();

		// Old content is never re-persisted: the original turn-1 user entry
		// (exact JSON string) appears exactly once, and the compaction entry
		// stays unique.
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(&dir).len() < 5 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("resumed turn should persist");
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(log.matches("\"first question\"").count(), 1);
		assert_eq!(log.matches("\"type\":\"compaction\"").count(), 1);
		let roles: Vec<&str> = read_session_messages(&dir)
			.iter()
			.map(Message::role)
			.collect();
		assert_eq!(roles, ["user", "user", "assistant", "user", "assistant"]);
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// /compact while a run is active is refused with a notice (upstream
	/// semantics for this port: never discard an in-flight turn); once idle,
	/// the same command compacts. A prompt arriving while compaction runs is
	/// queued and starts afterwards.
	#[tokio::test]
	async fn compact_during_run_rejected_then_idle_compact_queues_prompts() {
		let dir = temp_session_dir("compact-busy");
		// A gated provider keeps the run active until the test opens it.
		let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
		let agent = Agent::new(AgentOptions {
			stream_fn: Some(gated_stream_fn(gate_rx)),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);

		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "long running".into(),
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			while !agent.is_streaming() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();

		handle.cmd_tx.send(SessionCommand::Compact).unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text)) if text.starts_with("cannot compact") => break,
					Ok(SessionEvent::Notice(text)) if text.starts_with("compacted:") => {
						panic!("compaction must be refused while a run is active")
					}
					Ok(_) => {}
					Err(e) => panic!("event stream error: {e}"),
				}
			}
		})
		.await
		.expect("rejection notice should arrive");
		assert_eq!(agent.snapshot().messages.len(), 2); // untouched transcript
		assert!(compaction_entry_values(&dir).is_empty());

		// Finish the run, then compact; queue a prompt while compacting.
		gate_tx.send(true).unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle.cmd_tx.send(SessionCommand::Compact).unwrap();
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "queued during compaction".into(),
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text)) if text.starts_with("compacted:") => break,
					Ok(SessionEvent::Notice(text)) if text.contains("compaction failed") => {
						panic!("compaction failed: {text}")
					}
					Ok(_) => {}
					Err(e) => panic!("event stream error: {e}"),
				}
			}
		})
		.await
		.expect("idle compaction should finish");
		wait_for_agent_ends(&mut handle, 1).await;

		// The queued prompt ran after the summary, in order.
		let messages = agent.snapshot().messages;
		let roles: Vec<&str> = messages.iter().map(Message::role).collect();
		assert_eq!(roles, ["system", "user", "user", "assistant"]);
		let Message::User(summary) = &messages[1] else {
			panic!("expected summary message")
		};
		assert!(summary.content.as_text().contains("[User]: long running"));
		let Message::User(prompt) = &messages[2] else {
			panic!("expected queued prompt")
		};
		assert_eq!(prompt.content.as_text(), "queued during compaction");
		let entries = compaction_entry_values(&dir);
		assert_eq!(entries.len(), 1);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Provider whose single response waits on a watch channel (or reports
	/// abort on cancellation), so tests control exactly when a run ends. It
	/// echoes the last user message, so summarization results carry the
	/// serialized conversation back to the assertions.
	fn gated_stream_fn(gate: tokio::sync::watch::Receiver<bool>) -> pi_ai::StreamFn {
		Arc::new(
			move |model: &pi_ai::Model,
			      context: &pi_ai::TranscriptContext,
			      options: pi_ai::StreamOptions| {
				let mut gate = gate.clone();
				let signal = options.signal;
				let model = model.clone();
				let prompt_text = last_user_text(context);
				Box::pin(futures::stream::once(async move {
					loop {
						tokio::select! {
							_ = gate.changed() => {
								if *gate.borrow() {
									break;
								}
							}
							_ = signal.cancelled() => {
								return pi_ai::AssistantMessageEvent::Error {
									reason: pi_ai::StopReason::Aborted,
									error: pi_ai::AssistantMessage::error(
										&model,
										pi_ai::StopReason::Aborted,
										"aborted by user",
									),
								};
							}
						}
					}
					pi_ai::AssistantMessageEvent::Done {
						reason: pi_ai::StopReason::Stop,
						message: echo_assistant_message(
							&model,
							format!("[gated echo] {prompt_text}"),
						),
					}
				})) as pi_ai::AssistantMessageEventStream
			},
		)
	}

	async fn wait_for_agent_ends(handle: &mut SessionHandle, count: usize) {
		tokio::time::timeout(Duration::from_secs(5), async {
			let mut completed = 0;
			while completed < count {
				if let Ok(SessionEvent::Agent(event)) = handle.events.recv().await
					&& matches!(*event, AgentEvent::AgentEnd { .. })
				{
					completed += 1;
				}
			}
		})
		.await
		.expect("runs should complete in time");
	}

	#[tokio::test]
	async fn queued_prompts_persist_in_order() {
		let dir = temp_session_dir("ordered");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let session = SessionManager::open(&dir).unwrap();
		let mut handle = spawn_agent_session(agent, session, registry);
		for text in ["one", "two", "three"] {
			handle
				.cmd_tx
				.send(SessionCommand::Prompt { text: text.into() })
				.unwrap();
		}
		wait_for_agent_ends(&mut handle, 3).await;

		let messages = read_session_messages(&dir);
		let roles: Vec<&str> = messages.iter().map(Message::role).collect();
		assert_eq!(
			roles,
			[
				"user",
				"assistant",
				"user",
				"assistant",
				"user",
				"assistant"
			]
		);
		let users: Vec<String> = messages
			.iter()
			.filter_map(|m| match m {
				Message::User(u) => Some(u.content.as_text()),
				_ => None,
			})
			.collect();
		assert_eq!(users, vec!["one", "two", "three"]);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn write_failure_notifies_without_blocking_runs() {
		let dir = temp_session_dir("iofail");
		let path = dir.join("readonly.jsonl");
		std::fs::write(&path, "").unwrap();
		let readonly = std::fs::File::open(&path).unwrap();
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let session = SessionManager::with_file(dir.clone(), readonly);
		let mut handle = spawn_agent_session(agent, session, registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "hello".into(),
			})
			.unwrap();

		let (writes_failed, ended) = tokio::time::timeout(Duration::from_secs(5), async {
			let mut writes_failed = false;
			let mut ended = false;
			while !(writes_failed && ended) {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text)) if text.contains("session write failed") => {
						writes_failed = true;
					}
					Ok(SessionEvent::Agent(event))
						if matches!(*event, AgentEvent::AgentEnd { .. }) =>
					{
						ended = true;
					}
					Ok(_) => {}
					Err(e) => panic!("event stream error: {e}"),
				}
			}
			(writes_failed, ended)
		})
		.await
		.expect("failure notice and agent end should arrive in time");
		assert!(
			writes_failed && ended,
			"run must survive a broken session log"
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	// -- session replacement --------------------------------------------------

	/// Wait for the `session → …` replacement notice.
	async fn wait_for_replacement(handle: &mut SessionHandle) -> String {
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text)) if text.starts_with("session → ") => {
						return text;
					}
					Ok(_) => {}
					Err(e) => panic!("event stream error: {e}"),
				}
			}
		})
		.await
		.expect("replacement should finish in time")
	}

	fn current_session_file(handle: &SessionHandle) -> Option<PathBuf> {
		handle.shared.current_session_file.lock().unwrap().clone()
	}

	#[tokio::test]
	async fn new_session_starts_a_fresh_log_and_the_old_file_stops_growing() {
		let dir = temp_session_dir("new-session");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "old question".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(&dir).len() < 2 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("old tail should persist");
		assert_eq!(read_session_messages(&dir).len(), 2);

		handle
			.cmd_tx
			.send(SessionCommand::NewSession {
				parent_session: None,
			})
			.unwrap();
		let notice = wait_for_replacement(&mut handle).await;
		let new_file = current_session_file(&handle).expect("persisted replacement");
		assert_ne!(new_file, dir.join("session.jsonl"), "a fresh session file");
		assert!(notice.contains(&new_file.display().to_string()));

		// The live agent starts empty (fresh system message) and the next
		// prompt lands in the NEW log; the old file never grows again.
		let live = handle.shared.live_agent.lock().unwrap().clone();
		assert_eq!(
			live.snapshot().messages.len(),
			1,
			"only the fresh system message"
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "new question".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(new_file.parent().unwrap()).len() < 2 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("new session should persist the turn");
		assert_eq!(read_session_messages(&dir).len(), 2, "old file untouched");
		let (_, old_entries) = session_manager::load_session_file(&dir.join("session.jsonl"));
		assert!(
			old_entries.iter().all(|entry| entry.id().is_some()),
			"old log entries stay intact"
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
		std::fs::remove_dir_all(new_file.parent().unwrap()).unwrap();
	}

	#[tokio::test]
	async fn new_session_records_the_parent_lineage() {
		let dir = temp_session_dir("new-session-parent");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent, SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::NewSession {
				parent_session: Some(dir.join("session.jsonl").display().to_string()),
			})
			.unwrap();
		wait_for_replacement(&mut handle).await;
		let new_file = current_session_file(&handle).unwrap();
		let (header, _) = session_manager::load_session_file(&new_file);
		assert_eq!(
			header.expect("persisted").parent_session,
			Some(dir.join("session.jsonl").display().to_string()),
			"the new header carries parentSession"
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
		std::fs::remove_dir_all(new_file.parent().unwrap()).unwrap();
	}

	#[tokio::test]
	async fn switch_session_restores_the_target_transcript_and_cwd() {
		// Target session with its own history, in a directory whose header
		// cwd exists (the self-healing open stamps the process cwd).
		let target_dir = temp_session_dir("switch-target");
		{
			let mut target = SessionManager::open(&target_dir).unwrap();
			target
				.append_message(&Message::user("target history"))
				.unwrap();
		}
		let live_dir = temp_session_dir("switch-live");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(
			agent.clone(),
			SessionManager::open(&live_dir).unwrap(),
			registry,
		);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "live history".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;

		handle
			.cmd_tx
			.send(SessionCommand::SwitchSession {
				path: target_dir.display().to_string(),
			})
			.unwrap();
		wait_for_replacement(&mut handle).await;
		let file = current_session_file(&handle).unwrap();
		assert_eq!(
			file.parent().unwrap(),
			target_dir,
			"the live slots follow the target"
		);
		// The restored transcript seeds the fresh agent (plus the new system
		// message pi-agent seeds ahead of it).
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let live = handle.shared.live_agent.lock().unwrap().clone();
				let has_target = live.snapshot().messages.iter().any(
					|m| matches!(m, Message::User(u) if u.content.as_text() == "target history"),
				);
				if has_target {
					break;
				}
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("the target transcript should be restored");
		// A prompt appends to the target log, never to the old one.
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "after switch".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(&target_dir).len() < 3 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("the switched session should persist");
		assert_eq!(
			read_session_messages(&live_dir).len(),
			2,
			"the outgoing session file is untouched"
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(target_dir).unwrap();
		std::fs::remove_dir_all(live_dir).unwrap();
	}

	#[tokio::test]
	async fn fork_before_branches_from_the_message_parent() {
		let dir = temp_session_dir("fork-before");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		for text in ["first question", "second question"] {
			handle
				.cmd_tx
				.send(SessionCommand::Prompt { text: text.into() })
				.unwrap();
			wait_for_agent_ends(&mut handle, 1).await;
		}
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(&dir).len() < 4 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("both turns should persist");

		// The second user message's entry id (Before re-runs from its parent).
		let second_user_id = {
			let guard = handle.log.lock().unwrap();
			guard
				.session
				.entries()
				.iter()
				.filter(|entry| match &entry.kind {
					EntryKind::Message { message } => {
						matches!(message.as_ref(), Message::User(u) if u.content.as_text() == "second question")
					}
					_ => false,
				})
				.find_map(|entry| entry.id().map(str::to_owned))
				.expect("the second user entry exists")
		};
		handle
			.cmd_tx
			.send(SessionCommand::Fork {
				entry_id: second_user_id,
				position: ForkPosition::Before,
			})
			.unwrap();
		wait_for_replacement(&mut handle).await;
		let new_file = current_session_file(&handle).unwrap();
		// The branched file: header + parentSession + the path up to the
		// fork point (model/thinking entries + first turn).
		let (header, _entries) = session_manager::load_session_file(&new_file);
		assert_eq!(
			header.as_ref().map(|header| header.parent_session.clone()),
			Some(Some(dir.join("session.jsonl").display().to_string())),
			"the branched header names the source session file (file: {}, first line: {})",
			new_file.display(),
			std::fs::read_to_string(&new_file)
				.map(|text| text.lines().next().map(str::to_owned).unwrap_or_default())
				.unwrap_or_else(|error| format!("unreadable: {error}")),
		);
		let restored = restore_initial_messages(new_file.parent().unwrap());
		let roles: Vec<&str> = restored.iter().map(Message::role).collect();
		assert_eq!(
			roles,
			["user", "assistant"],
			"the fork replays up to the second question's parent"
		);
		let Message::User(first) = &restored[0] else {
			panic!("expected the first turn")
		};
		assert_eq!(first.content.as_text(), "first question");
		// A prompt appends to the branched file.
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "edited question".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(new_file.parent().unwrap()).len() < 4 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("the branched session should persist");
		let restored = read_session_messages(new_file.parent().unwrap());
		let users: Vec<String> = restored
			.iter()
			.filter_map(|m| match m {
				Message::User(u) => Some(u.content.as_text().to_string()),
				_ => None,
			})
			.collect();
		assert_eq!(users, ["first question", "edited question"]);
		// The source log keeps all four turns.
		assert_eq!(read_session_messages(&dir).len(), 4);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
		std::fs::remove_dir_all(new_file.parent().unwrap()).unwrap();
	}

	#[tokio::test]
	async fn fork_on_an_unsaved_session_is_refused() {
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent, SessionManager::in_memory(), registry);
		// Seed one entry so a leaf exists, then fork at it.
		{
			let mut guard = handle.log.lock().unwrap();
			guard
				.session
				.append_message(&Message::user("unsaved"))
				.unwrap();
		}
		let leaf = {
			let guard = handle.log.lock().unwrap();
			guard.session.leaf_id().map(str::to_owned).unwrap()
		};
		handle
			.cmd_tx
			.send(SessionCommand::Fork {
				entry_id: leaf,
				position: ForkPosition::At,
			})
			.unwrap();
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				match handle.events.recv().await {
					Ok(SessionEvent::Notice(text)) if text.contains("has not been saved yet") => {
						break;
					}
					Ok(SessionEvent::Notice(text)) if text.starts_with("session → ") => {
						panic!("an unsaved session must not be forked: {text}")
					}
					Ok(_) => {}
					Err(e) => panic!("event stream error: {e}"),
				}
			}
		})
		.await
		.expect("the refusal notice should arrive");
		assert!(current_session_file(&handle).is_none(), "still unpersisted");
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
	}

	// -- bash passthrough -----------------------------------------------------

	async fn next_wire_event(handle: &mut SessionHandle) -> WireEvent {
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				match handle.wire_events.recv().await {
					Ok(event) => return event,
					Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
					Err(e) => panic!("wire stream error: {e}"),
				}
			}
		})
		.await
		.expect("wire event should arrive in time")
	}

	#[tokio::test]
	async fn bash_passthrough_streams_persists_and_joins_context() {
		let dir = temp_session_dir("bash-roundtrip");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::ExecuteBash {
				command: "printf hello-bash".into(),
				exclude_from_context: false,
				id: Some(serde_json::json!("b1")),
			})
			.unwrap();
		// Streamed deltas, then the done event carrying the upstream result.
		let mut updates = 0;
		let result = loop {
			match next_wire_event(&mut handle).await {
				WireEvent::BashExecutionUpdate { id, delta } => {
					updates += 1;
					assert_eq!(id, Some(serde_json::json!("b1")));
					assert!(!delta.is_empty());
				}
				WireEvent::BashExecutionDone { id, result } => {
					assert_eq!(id, Some(serde_json::json!("b1")));
					break result;
				}
				other => panic!("unexpected wire event: {other:?}"),
			}
		};
		assert!(updates >= 1, "at least one delta streams");
		assert_eq!(result.exit_code, Some(0));
		assert!(!result.cancelled);
		assert!(result.output.contains("hello-bash"), "{}", result.output);

		// Persisted as the upstream bashExecution message entry, and the
		// user-form text joined the live transcript.
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let has_entry = {
					let guard = handle.log.lock().unwrap();
					guard
						.session
						.entries()
						.iter()
						.any(|entry| matches!(entry.kind, EntryKind::BashExecution { .. }))
				};
				let joined = agent.snapshot().messages.iter().any(
					|m| matches!(m, Message::User(u) if u.content.as_text().contains("Ran `printf hello-bash`")),
				);
				if has_entry && joined {
					break;
				}
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("the bash result should persist and join the context");
		// The live cursor covers the appended context message: a follow-up
		// prompt never re-persists it as a message entry.
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "after bash".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		// Projection: the bash text replay + the follow-up turn.
		tokio::time::timeout(Duration::from_secs(5), async {
			while read_session_messages(&dir).len() < 3 {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("the follow-up turn should persist");
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(
			log.matches("Ran `printf hello-bash`").count(),
			0,
			"the bash context text is never re-persisted as a message entry"
		);
		assert_eq!(log.matches("\"role\":\"bashExecution\"").count(), 1);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn bash_exclude_from_context_persists_without_joining_context() {
		let dir = temp_session_dir("bash-excluded");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::ExecuteBash {
				command: "printf secret-output".into(),
				exclude_from_context: true,
				id: None,
			})
			.unwrap();
		let result = loop {
			match next_wire_event(&mut handle).await {
				WireEvent::BashExecutionDone { result, .. } => break result,
				WireEvent::BashExecutionUpdate { .. } => continue,
				other => panic!("unexpected wire event: {other:?}"),
			}
		};
		assert!(result.output.contains("secret-output"));
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let has_entry = {
					let guard = handle.log.lock().unwrap();
					guard
						.session
						.entries()
						.iter()
						.any(|entry| matches!(entry.kind, EntryKind::BashExecution { .. }))
				};
				if has_entry {
					break;
				}
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		})
		.await
		.expect("the excluded result should persist");
		// On disk, but the live transcript never grows and the projection
		// skips the excluded entry.
		assert_eq!(
			agent.snapshot().messages.len(),
			1,
			"excluded output stays out of the live context"
		);
		assert_eq!(read_session_messages(&dir).len(), 0);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn abort_bash_cancels_the_running_command() {
		let dir = temp_session_dir("bash-abort");
		let agent = mock_agent();
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent, SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::ExecuteBash {
				command: "sleep 30".into(),
				exclude_from_context: false,
				id: None,
			})
			.unwrap();
		// Wait for the command to actually start producing nothing (it is
		// running), then abort it.
		tokio::time::sleep(Duration::from_millis(200)).await;
		handle.cmd_tx.send(SessionCommand::AbortBash).unwrap();
		let result = loop {
			match next_wire_event(&mut handle).await {
				WireEvent::BashExecutionDone { result, .. } => break result,
				WireEvent::BashExecutionUpdate { .. } => continue,
				other => panic!("unexpected wire event: {other:?}"),
			}
		};
		assert!(result.cancelled, "the abort must surface as cancelled");
		assert_eq!(result.exit_code, None);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	// -- auto-retry -----------------------------------------------------------

	/// Scripted provider: summarization echoes; ordinary requests follow
	/// `script(turn)`. Turn 1 is the retryable failure unless `first_ok`.
	fn retry_stream_fn(
		script: impl Fn(usize, &pi_ai::Model) -> pi_ai::AssistantMessageEvent + Send + Sync + 'static,
	) -> (pi_ai::StreamFn, Arc<std::sync::Mutex<usize>>) {
		let script = Arc::new(script);
		let requests = Arc::new(std::sync::Mutex::new(0usize));
		let counter = requests.clone();
		let stream: pi_ai::StreamFn = Arc::new(move |model, context, _| {
			if is_summarization_request(context) {
				let model = model.clone();
				let text = format!("[mock summary] {}", last_user_text(context));
				return Box::pin(futures::stream::once(async move {
					pi_ai::AssistantMessageEvent::Done {
						reason: StopReason::Stop,
						message: echo_assistant_message(&model, text),
					}
				})) as pi_ai::AssistantMessageEventStream;
			}
			let turn = {
				let mut count = counter.lock().unwrap();
				*count += 1;
				*count
			};
			let script = script.clone();
			let model = model.clone();
			Box::pin(futures::stream::once(async move { script(turn, &model) }))
				as pi_ai::AssistantMessageEventStream
		});
		(stream, requests)
	}

	fn retryable_error(model: &pi_ai::Model) -> pi_ai::AssistantMessageEvent {
		let mut message = echo_assistant_message(model, String::new());
		message.stop_reason = StopReason::Error;
		message.error_message = Some("fetch failed: connection refused".into());
		pi_ai::AssistantMessageEvent::Error {
			reason: StopReason::Error,
			error: message,
		}
	}

	#[tokio::test]
	async fn retryable_error_schedules_a_retry_and_success_resets() {
		let dir = temp_session_dir("auto-retry");
		let (stream, requests) = retry_stream_fn(|turn, model| match turn {
			1 => retryable_error(model),
			_ => stop_reply(model, "recovered answer"),
		});
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(agent.clone(), SessionManager::open(&dir).unwrap(), registry);
		handle.shared.retry.lock().unwrap().base_delay_ms = 10;

		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "first".into(),
			})
			.unwrap();
		let notices = notices_until_agent_ends(&mut handle, 2).await;
		assert_eq!(
			*requests.lock().unwrap(),
			2,
			"the failed request repeats once"
		);

		// Wire sequence: agent_end carries willRetry=true, the backoff is
		// announced, and the successful retry closes the sequence.
		let mut saw_start = false;
		let mut saw_success_end = false;
		while !(saw_start && saw_success_end) {
			match next_wire_event(&mut handle).await {
				WireEvent::AutoRetryStart {
					attempt,
					max_attempts,
					delay_ms,
					error_message,
				} => {
					saw_start = true;
					assert_eq!(attempt, 1);
					assert_eq!(max_attempts, retry_policy::RETRY_MAX_RETRIES);
					assert_eq!(delay_ms, 10);
					assert!(error_message.contains("connection refused"));
				}
				WireEvent::AutoRetryEnd {
					success,
					attempt,
					final_error,
				} => {
					saw_success_end = true;
					assert!(success);
					assert_eq!(attempt, 1);
					assert_eq!(final_error, None);
				}
				_ => {}
			}
		}
		// The agent_end decisions: true for the failed run, false after.
		let decisions: Vec<bool> = {
			let mut guard = handle.shared.agent_end_will_retry.lock().unwrap();
			guard.drain(..).collect()
		};
		assert_eq!(
			decisions,
			vec![true, false],
			"the failed run predicts a retry; the retry run does not"
		);
		let _ = notices;

		// Transcript: the failed attempt was omitted (tombstoned) and the
		// retry replayed the turn.
		let snapshot = agent.snapshot();
		let roles: Vec<&str> = snapshot.messages.iter().map(Message::role).collect();
		assert_eq!(roles, ["system", "user", "assistant"]);
		let Message::Assistant(last) = snapshot.messages.last().unwrap() else {
			panic!("expected the retried reply")
		};
		assert_eq!(last.text(), "recovered answer");
		// The counter reset on the successful assistant.
		assert_eq!(handle.shared.retry.lock().unwrap().attempt, 0);
		// The failed attempt is written exactly once and tombstoned.
		let log = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(log.matches("connection refused").count(), 1);
		assert_eq!(log.matches("\"type\":\"context_edit\"").count(), 1);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn retry_budget_exhaustion_reports_the_final_failure() {
		let dir = temp_session_dir("retry-budget");
		let (stream, requests) = retry_stream_fn(|_turn, model| retryable_error(model));
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent, SessionManager::open(&dir).unwrap(), registry);
		handle.shared.retry.lock().unwrap().base_delay_ms = 5;

		handle
			.cmd_tx
			.send(SessionCommand::Prompt { text: "go".into() })
			.unwrap();
		// Three retries are scheduled, then the sequence reports failure.
		tokio::time::timeout(Duration::from_secs(30), async {
			loop {
				if let WireEvent::AutoRetryEnd {
					success: false,
					attempt: 3,
					..
				} = next_wire_event(&mut handle).await
				{
					break;
				}
			}
		})
		.await
		.expect("the retry sequence should exhaust and report");
		assert_eq!(
			*requests.lock().unwrap(),
			4,
			"initial attempt plus three retries"
		);
		assert_eq!(
			handle.shared.retry.lock().unwrap().attempt,
			0,
			"reset after the final failure"
		);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[tokio::test]
	async fn abort_retry_cancels_the_pending_backoff() {
		let dir = temp_session_dir("retry-abort");
		let (stream, requests) = retry_stream_fn(|_turn, model| retryable_error(model));
		let agent = Agent::new(AgentOptions {
			model: recovery_model(),
			stream_fn: Some(stream),
			..Default::default()
		});
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle = spawn_agent_session(agent, SessionManager::open(&dir).unwrap(), registry);
		// A long backoff gives the test a window to abort it.
		handle.shared.retry.lock().unwrap().base_delay_ms = 60_000;

		handle
			.cmd_tx
			.send(SessionCommand::Prompt { text: "go".into() })
			.unwrap();
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				if let WireEvent::AutoRetryStart { .. } = next_wire_event(&mut handle).await {
					break;
				}
			}
		})
		.await
		.expect("the retry should be scheduled");
		handle.cmd_tx.send(SessionCommand::AbortRetry).unwrap();
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				if let WireEvent::AutoRetryEnd {
					success,
					attempt,
					final_error,
				} = next_wire_event(&mut handle).await
				{
					assert!(!success);
					assert_eq!(attempt, 1);
					assert_eq!(final_error.as_deref(), Some("Retry cancelled"));
					break;
				}
			}
		})
		.await
		.expect("the cancellation should be reported");
		assert_eq!(handle.shared.retry.lock().unwrap().attempt, 0);
		// The cancelled retry never issued a second request.
		assert_eq!(*requests.lock().unwrap(), 1);
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// Resume flow as main.rs drives it: seed `initial_messages` from the
	/// previous log, reopen the directory, and keep appending.
	#[tokio::test]
	async fn resumed_session_appends_without_repersisting_old_messages() {
		let dir = temp_session_dir("resume");

		// First run: persist one exchange, then "exit".
		{
			let agent = Agent::new(AgentOptions {
				system_prompt: Some("first run".into()),
				stream_fn: Some(agent_stream_fn()),
				..AgentOptions::default()
			});
			let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
			let session = SessionManager::open(&dir).unwrap();
			let mut handle = spawn_agent_session(agent, session, registry);
			handle
				.cmd_tx
				.send(SessionCommand::Prompt {
					text: "old question".into(),
				})
				.unwrap();
			wait_for_agent_ends(&mut handle, 1).await;
			handle.cmd_tx.send(SessionCommand::Exit).unwrap();
		}

		// Resumed run: restored transcript + fresh system prompt.
		let restored = restore_initial_messages(&dir);
		let roles: Vec<&str> = restored.iter().map(Message::role).collect();
		assert_eq!(roles, ["user", "assistant"]);
		let resumed = Agent::new(AgentOptions {
			system_prompt: Some("resumed run".into()),
			stream_fn: Some(agent_stream_fn()),
			initial_messages: restored,
			..AgentOptions::default()
		});
		// The fresh system message is seeded ahead of the restored transcript.
		assert_eq!(resumed.snapshot().messages[0].role(), "system");
		let registry = Arc::new(std::sync::Mutex::new(ModelRegistry::load(None)));
		let mut handle =
			spawn_agent_session(resumed, SessionManager::open(&dir).unwrap(), registry);
		handle
			.cmd_tx
			.send(SessionCommand::Prompt {
				text: "new question".into(),
			})
			.unwrap();
		wait_for_agent_ends(&mut handle, 1).await;
		handle.cmd_tx.send(SessionCommand::Exit).unwrap();

		let messages = read_session_messages(&dir);
		let roles: Vec<&str> = messages.iter().map(Message::role).collect();
		assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
		// Old messages appear exactly once (never re-persisted), new ones append.
		let users: Vec<String> = messages
			.iter()
			.filter_map(|m| match m {
				Message::User(u) => Some(u.content.as_text().to_string()),
				_ => None,
			})
			.collect();
		assert_eq!(users, ["old question", "new question"]);
		let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(text.matches("\"role\":\"user\"").count(), 2);
		// The fresh system prompt is never persisted.
		assert!(!text.contains("resumed run"));
		std::fs::remove_dir_all(dir).unwrap();
	}
}
