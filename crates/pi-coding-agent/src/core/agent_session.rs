//! Agent session — port of
//! `pi/packages/coding-agent/src/core/agent-session.ts` (reduced to the
//! framework-stage surface).
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
//! Not ported yet (listed against the upstream source):
//! - bash passthrough, session switch/fork, and the extension hook surface.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;

use pi_agent::{Agent, AgentEvent, PromptInput};
use pi_ai::{AssistantMessage, Message, StopReason, ThinkingLevel};

use super::compaction::{self, CompactionOutcome};
use super::event_bus::{EventBus, EventBusWeak};
use super::model_registry::ModelRegistry;
use super::session_manager::{self, EntryKind, SessionEntry, SessionManager};
use super::settings_manager::CompactionSettings;

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
	Exit,
}

/// Events the session emits to modes.
///
/// Compaction reports through `Notice` (upstream has dedicated
/// `compaction_start`/`compaction_end` session events; modes here match
/// `SessionEvent` exhaustively, so a dedicated variant lands with the phase
/// that owns them). Notices use stable prefixes: `compacted:`,
/// `compaction failed:`, `cannot compact:`.
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
	Settled,
}

/// Handle held by modes; the session task runs in the background.
pub struct SessionHandle {
	pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
	pub events: tokio::sync::broadcast::Receiver<SessionEvent>,
	/// Shared agent for status probes (snapshot, is_streaming, abort).
	pub agent: Agent,
	pub session_dir: Option<std::path::PathBuf>,
	/// Mirrors the auto-compaction toggle for lock-free status probes (TUI
	/// footer indicator); the session task owns the authoritative state.
	pub auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	/// True while a compaction summarization is in flight (RPC `get_state`'s
	/// `isCompacting`); the session task owns the authoritative state.
	pub compacting: Arc<std::sync::atomic::AtomicBool>,
}

/// Session log plus the persist cursor, shared between the session task and
/// the mid-run compaction hook. Every append goes through one lock, which is
/// never held across an `await` (appends are synchronous fsync'd writes).
struct SharedLog {
	session: SessionManager,
	/// Transcript prefix already written to the log (the agent transcript
	/// index; the leading system message is never logged, so the cursor
	/// starts past it).
	persisted: usize,
	/// Compaction summarized + committed to the log by the mid-run hook,
	/// waiting for the run to end so the transcript swap becomes legal.
	pending_mid_run: Option<PendingMidRunCompaction>,
}

/// See [`SharedLog::pending_mid_run`].
struct PendingMidRunCompaction {
	outcome: CompactionOutcome,
}

type SharedLogRef = Arc<std::sync::Mutex<SharedLog>>;

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

	let session_dir = session.session_dir().map(|dir| dir.to_path_buf());
	if let Some(dir) = session.session_dir() {
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
	agent.set_session_id(session.session_id());
	// Upstream records the session's initial model/thinking as entries right
	// after the header (agent-session startup); without them a fresh Rust
	// session lacks the `model_change`/`thinking_level_change` chain the TS
	// pi always writes. Restored sessions keep their existing chain.
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
	let auto_compaction = Arc::new(std::sync::atomic::AtomicBool::new(
		settings.enabled.unwrap_or(true),
	));
	let compacting = Arc::new(std::sync::atomic::AtomicBool::new(false));

	let log: SharedLogRef = Arc::new(std::sync::Mutex::new(SharedLog {
		persisted: agent.snapshot().messages.len(),
		session,
		pending_mid_run: None,
	}));

	// Mid-run compaction (upstream `prepareNextTurn` between-turn check):
	// installed before the task starts so every run of this session carries
	// it. The closure clones the shared state; see [`MidRunHook::check`].
	let hook = Arc::new(MidRunHook {
		agent: agent.clone(),
		log: log.clone(),
		auto_compaction: auto_compaction.clone(),
		compacting: compacting.clone(),
		settings: settings.clone(),
		bus: event_bus.downgrade(),
	});
	agent.set_prepare_next_turn(Arc::new(move |_messages| {
		let hook = hook.clone();
		Box::pin(async move {
			hook.check().await;
			None
		})
	}));

	tokio::spawn(session_task(
		agent.clone(),
		log,
		registry,
		cmd_rx,
		event_bus.clone(),
		auto_compaction.clone(),
		compacting.clone(),
		settings,
	));

	SessionHandle {
		cmd_tx,
		events: event_bus.subscribe(),
		agent,
		session_dir,
		auto_compaction,
		compacting,
	}
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
	agent: Agent,
	log: SharedLogRef,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
	event_bus: EventBus<SessionEvent>,
	auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	compacting_flag: Arc<std::sync::atomic::AtomicBool>,
	settings: CompactionSettings,
) {
	let mut agent_rx = agent.subscribe();
	let mut pending = VecDeque::new();
	let mut run: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
	// In-flight compaction summarization; the agent is idle while this runs.
	let mut compacting: Option<tokio::task::JoinHandle<anyhow::Result<CompactionOutcome>>> = None;
	let mut compaction_commit_failed = false;
	let mut recovery_compaction = false;
	let mut overflow_attempted = false;
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
			event_bus.emit(SessionEvent::Settled);
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
						let stopped_compaction = if let Some(task) = compacting.take() {
							task.abort();
							let _ = task.await;
							true
						} else { false };
						agent.abort();
						notice(&event_bus, "aborting…".into());
						if stopped_compaction && run.is_none() && !agent.is_streaming()
							&& let Some(text) = pending.pop_front() {
							overflow_attempted = false;
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
					SessionCommand::Exit => break,
				}
			}
			result = async { run.as_mut().expect("guarded run").await }, if run.is_some() => {
				run = None;
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
				if compacting.is_none() && run.is_none() && !agent.is_streaming() && let Some(text) = pending.pop_front() {
					overflow_attempted = false;
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
/// no room left for output.
fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
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
			})),
			registry,
			rx,
			bus,
			Arc::new(std::sync::atomic::AtomicBool::new(true)),
			Arc::new(std::sync::atomic::AtomicBool::new(false)),
			CompactionSettings::default(),
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
