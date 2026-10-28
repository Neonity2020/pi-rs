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
//! while idle and after a run completes, and a context-overflow or
//! recoverable-length tail attempt is omitted (see [`omit_failed_attempt`]),
//! compacted, and retried exactly once.
//!
//! Not ported yet (listed against the upstream source):
//! - mid-run compaction checks before the next assistant request
//!   (`agent.ts` `prepareNextTurn` hook — pi-agent has no such hook), listed
//!   as follow-up;
//! - bash passthrough, session switch/fork, and the extension hook surface.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use pi_agent::{Agent, AgentEvent, PromptInput};
use pi_ai::{AssistantMessage, Message, StopReason, ThinkingLevel};

use super::compaction::{self, CompactionOutcome};
use super::event_bus::EventBus;
use super::model_registry::ModelRegistry;
use super::session_manager::{SessionEntry, SessionManager};
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
	session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	settings: CompactionSettings,
) -> SessionHandle {
	let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
	let event_bus = EventBus::<SessionEvent>::new();

	let session_dir = session.session_dir().map(|dir| dir.to_path_buf());
	if let Some(dir) = session.session_dir() {
		let (projection, floor) = read_session_projection(dir);
		if floor > 0 {
			let snapshot = agent.snapshot();
			let ordinary: Vec<_> = snapshot
				.messages
				.iter()
				.filter(|m| !matches!(m, Message::System(_)))
				.collect();
			if ordinary.len() == projection.len()
				&& ordinary
					.iter()
					.zip(&projection)
					.all(|(a, (_, b))| message_projection_eq(a, b))
			{
				let leading_systems = snapshot.messages.len() - ordinary.len();
				agent.set_usage_valid_from(leading_systems + floor);
			}
		}
	}
	agent.set_session_id(session.session_id());
	let auto_compaction = Arc::new(std::sync::atomic::AtomicBool::new(
		settings.enabled.unwrap_or(true),
	));

	tokio::spawn(session_task(
		agent.clone(),
		session,
		registry,
		cmd_rx,
		event_bus.clone(),
		auto_compaction.clone(),
		settings,
	));

	SessionHandle {
		cmd_tx,
		events: event_bus.subscribe(),
		agent,
		session_dir,
		auto_compaction,
	}
}

async fn session_task(
	agent: Agent,
	mut session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
	event_bus: EventBus<SessionEvent>,
	auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	settings: CompactionSettings,
) {
	let mut agent_rx = agent.subscribe();
	let mut persisted = agent.snapshot().messages.len();
	let mut pending = VecDeque::new();
	let mut run: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
	// In-flight compaction summarization; the agent is idle while this runs.
	let mut compacting: Option<tokio::task::JoinHandle<anyhow::Result<CompactionOutcome>>> = None;
	let mut compaction_commit_failed = false;
	let mut recovery_compaction = false;
	let mut overflow_attempted = false;

	let notice = |bus: &EventBus<SessionEvent>, text: String| {
		bus.emit(SessionEvent::Notice(text));
	};

	loop {
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
							&& let Some(cut) = auto_compaction_cut(&agent, &session, persisted, settings.keep_recent_tokens.unwrap_or(20_000)) {
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
								agent.set_stream_fn(pi_ai::providers::stream_fn_for(&model));
								agent.set_model(model.clone());
								registry.lock().unwrap().set_default(&model);
								if let Err(e) = session.append(&SessionEntry::ModelChange { model: model.key() }) {
									notice(&event_bus, format!("session write failed: {e}"));
								}
								notice(&event_bus, format!("model → {}", model.key()));
							}
							None => notice(&event_bus, format!("no model matching `{query}`")),
						}
					}
					SessionCommand::SetThinking { level } => {
						agent.set_thinking_level(level);
						notice(&event_bus, format!("thinking → {level:?}"));
					}
					SessionCommand::CycleThinking => {
						let next = next_thinking_level(agent.thinking_level());
						agent.set_thinking_level(next);
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
							persist_messages(&agent, &mut session, &mut persisted, &event_bus);
						}
						if run.is_some() || agent.is_streaming() {
							// Upstream aborts the run before compacting; this port
							// refuses instead so a slash command never discards an
							// in-flight turn.
							notice(&event_bus, "cannot compact: agent run active".into());
						} else if compacting.is_some() {
							notice(&event_bus, "compaction already running".into());
						} else {
							persist_messages(&agent, &mut session, &mut persisted, &event_bus);
							if persisted != agent.snapshot().messages.len() {
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
				persist_messages(&agent, &mut session, &mut persisted, &event_bus);
				let snapshot = agent.snapshot();
				let recover = auto_compaction.load(std::sync::atomic::Ordering::Relaxed)
					&& snapshot.messages.last().is_some_and(|m| matches!(m, Message::Assistant(a) if recoverable_overflow(a, &agent.model())));
				if recover && !overflow_attempted {
					if let Some(cut) = auto_compaction_cut(&agent, &session, persisted, settings.keep_recent_tokens.unwrap_or(20_000)) {
						match omit_failed_attempt(&agent, &mut session, &mut persisted) {
							Ok(()) => {
								overflow_attempted = true;
								recovery_compaction = true;
								notice(&event_bus, "context overflow: compacting before one retry".into());
								let agent = agent.clone();
								compacting = Some(tokio::spawn(async move { compaction::compact_prefix(&agent, Some(cut)).await }));
							}
							Err(error) => {
								compaction_commit_failed = true;
								notice(&event_bus, format!("overflow recovery commit failed: {error}; reopen to recover"));
								break;
							}
						}
					} else {
						notice(&event_bus, "overflow recovery unavailable: no durable compaction boundary".into());
					}
				} else if recover && overflow_attempted {
					notice(&event_bus, "overflow recovery failed after one compact-and-retry attempt".into());
				}
				if !recover && auto_compaction.load(std::sync::atomic::Ordering::Relaxed)
					&& let Some((tokens, window)) = super::context_usage::compute(&agent)
					&& tokens > window.saturating_sub(settings.reserve_tokens.unwrap_or(16_384))
					&& let Some(cut) = auto_compaction_cut(&agent, &session, persisted, settings.keep_recent_tokens.unwrap_or(20_000))
				{
					notice(&event_bus, "auto-compaction (threshold)…".into());
					let agent = agent.clone();
					compacting = Some(tokio::spawn(async move { compaction::compact_prefix(&agent, Some(cut)).await }));
				}
				if compacting.is_none() && let Some(text) = pending.pop_front() {
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
						if !apply_compaction(&agent, &mut session, &mut persisted, &event_bus, outcome) {
							compaction_commit_failed = true;
							notice(&event_bus, "session stopped after compaction commit failure; reopen to recover".into());
							break;
						}
						committed = true;
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
							persist_messages(&agent, &mut session, &mut persisted, &event_bus);
						}
						event_bus.emit(SessionEvent::Agent(Box::new(agent_event)));
					}
					Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
						notice(&event_bus, format!("event lag: dropped {count} events"));
								persist_messages(&agent, &mut session, &mut persisted, &event_bus);
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
		persist_messages(&agent, &mut session, &mut persisted, &event_bus);
	}
}

// The broadcast channel is lossy; the transcript is the authoritative source.
// Advance only after a successful append so a later event can retry a failed write.
fn persist_messages(
	agent: &Agent,
	session: &mut SessionManager,
	persisted: &mut usize,
	bus: &EventBus<SessionEvent>,
) {
	if let Err(error) = session.sync_pending_metadata() {
		bus.emit(SessionEvent::Notice(format!(
			"session write failed: {error}"
		)));
		return;
	}
	let snapshot = agent.snapshot();
	for message in snapshot.messages.iter().skip(*persisted) {
		match session.append_message(message) {
			Ok(()) => {
				*persisted += 1;
				if session.session_dir().is_some() {
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

/// Select a whole-turn boundary and verify it is durably addressable. Old
/// logs without message IDs are left untouched until they have enough newer
/// ID-bearing messages to form a safe boundary.
fn auto_compaction_cut(
	agent: &Agent,
	session: &SessionManager,
	persisted: usize,
	keep_tokens: u64,
) -> Option<(usize, Option<String>)> {
	let snapshot = agent.snapshot();
	if persisted != snapshot.messages.len() {
		return None;
	}
	let messages: Vec<Message> = snapshot
		.messages
		.iter()
		.filter(|m| !matches!(m, Message::System(_)))
		.cloned()
		.collect();
	// Upstream also cuts at assistant messages; this pipeline omits a failed
	// tail assistant before summarizing, so a cut landing on that tail would
	// dangle after the omission — clamp to the closest user boundary, which
	// starts the turn where the budget was exceeded (conservative because
	// split-turn cuts are not ported).
	let target = compaction::find_cut_point(&messages, keep_tokens);
	let cut = (0..=target)
		.rev()
		.find(|&index| matches!(messages[index], Message::User(_)))?;
	if cut == 0 {
		return None;
	}
	let id = if let Some(dir) = session.session_dir() {
		let (projection, _) = read_session_projection(dir);
		if projection.len() != messages.len() {
			return None;
		}
		if !projection
			.iter()
			.zip(messages.iter())
			.all(|((_, a), b)| message_projection_eq(a, b))
		{
			return None;
		}
		Some(projection[cut].0.clone()?)
	} else {
		None
	};
	Some((cut, id))
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
fn omit_failed_attempt(
	agent: &Agent,
	session: &mut SessionManager,
	persisted: &mut usize,
) -> anyhow::Result<()> {
	let snapshot = agent.snapshot();
	if *persisted != snapshot.messages.len() {
		anyhow::bail!("transcript is not fully persisted");
	}
	let Some(Message::Assistant(_)) = snapshot.messages.last() else {
		anyhow::bail!("transcript does not end with the failed assistant");
	};
	let Some(dir) = session.session_dir() else {
		anyhow::bail!("overflow recovery needs a session log");
	};
	let (projection, _) = read_session_projection(dir);
	let Some((entry_id, logged)) = projection.last() else {
		anyhow::bail!("session log is empty");
	};
	let Some(target_id) = entry_id.clone() else {
		anyhow::bail!("failed attempt has no log entry id");
	};
	if !message_projection_eq(logged, snapshot.messages.last().unwrap()) {
		anyhow::bail!("session log tail is not the failed attempt");
	}
	let mut without_attempt = snapshot.messages.clone();
	without_attempt.pop();
	let tombstone = SessionEntry::ContextEdit {
		target_id,
		replacement: None,
	};
	agent.replace_messages_transactionally(&snapshot.messages, without_attempt, || {
		session.append(&tombstone)?;
		Ok(())
	})?;
	// The failed attempt was persisted behind the old cursor; everything that
	// remains was persisted before it, so rebasing keeps the log append-only
	// without duplicating any of them.
	*persisted = agent.snapshot().messages.len();
	Ok(())
}

/// Commit a finished compaction: write the log entry first — it is the
/// authoritative record (upstream `SessionManager.appendCompaction`) — then
/// swap the agent history. A failed write leaves the transcript untouched, so
/// the log never claims a compaction the agent did not apply.
fn apply_compaction(
	agent: &Agent,
	session: &mut SessionManager,
	persisted: &mut usize,
	bus: &EventBus<SessionEvent>,
	outcome: CompactionOutcome,
) -> bool {
	let mut entry = SessionEntry::compaction(outcome.summary.clone(), outcome.tokens_before);
	if let (
		Some(id),
		SessionEntry::Compaction {
			first_kept_entry_id,
			..
		},
	) = (&outcome.first_kept_id, &mut entry)
	{
		*first_kept_entry_id = id.clone();
	}
	let mut history = compaction::replacement_history(&outcome.source_messages, &outcome.summary);
	history.extend(outcome.kept);
	let result = agent.replace_messages_transactionally(&outcome.source_messages, history, || {
		session.append(&entry)?;
		Ok(())
	});
	if let Err(error) = result {
		bus.emit(SessionEvent::Notice(format!(
			"compaction commit failed: {error}"
		)));
		return false;
	}
	if session.session_dir().is_some() {
		bus.emit(SessionEvent::Persisted { kind: "compaction" });
	}

	// The summary reached the log through the compaction entry, never as a
	// message entry: rebase the persist cursor onto the replaced transcript so
	// it is not double-persisted.
	*persisted = agent.snapshot().messages.len();
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

/// Rebuild a transcript from a session file (resume support).
///
/// Compaction entries replay as their summary message: this port summarizes
/// the whole non-system transcript ("keep nothing" cut), so everything
/// accumulated before the entry was compacted into it and is replaced by the
/// rebuilt summary — the flat-log equivalent of upstream's
/// `buildContextEntries` (`pi/packages/coding-agent/src/core/session-manager.ts`),
/// where a compaction entry contributes
/// `[systemMessage?, compactionSummary]` and the summarized older entries are
/// omitted. A compaction entry without a parsable summary cannot be rebuilt
/// and is skipped, keeping the accumulated prefix.
pub fn read_session_messages(dir: &std::path::Path) -> Vec<Message> {
	read_session_projection(dir)
		.0
		.into_iter()
		.map(|(_, message)| message)
		.collect()
}

fn read_session_projection(dir: &std::path::Path) -> (Vec<(Option<String>, Message)>, usize) {
	let Ok(text) = std::fs::read_to_string(dir.join("session.jsonl")) else {
		return (Vec::new(), 0);
	};
	let mut messages: Vec<(Option<String>, Message)> = Vec::new();
	let mut usage_valid_from = 0;
	for line in text.lines() {
		let Ok(value) = serde_json::from_str::<Value>(line) else {
			continue;
		};
		match value.get("type").and_then(Value::as_str) {
			Some("message") => {
				if let Some(value_message) = value.get("message")
					&& let Ok(message) = serde_json::from_value::<Message>(value_message.clone())
				{
					messages.push((
						value.get("id").and_then(Value::as_str).map(str::to_owned),
						message,
					));
				}
			}
			Some("context_edit") if value.get("replacement").is_some_and(Value::is_null) => {
				if let Some(target) = value.get("targetId").and_then(Value::as_str)
					&& let Some(index) = messages
						.iter()
						.position(|(id, _)| id.as_deref() == Some(target))
				{
					messages.remove(index);
					usage_valid_from = usage_valid_from.min(messages.len());
				}
			}
			Some("compaction") => {
				if let Some(summary) = value.get("summary").and_then(Value::as_str) {
					let id = value.get("id").and_then(Value::as_str);
					let first = value.get("firstKeptEntryId").and_then(Value::as_str);
					let kept = if let Some(first) = first.filter(|first| Some(*first) != id) {
						let Some(index) = messages
							.iter()
							.position(|(id, _)| id.as_deref() == Some(first))
						else {
							continue;
						};
						messages.split_off(index)
					} else {
						Vec::new()
					};
					messages = vec![(
						id.map(str::to_owned),
						compaction::compaction_summary_message(summary),
					)];
					messages.extend(kept);
					usage_valid_from = messages.len();
				}
			}
			_ => {}
		}
	}
	(messages, usage_valid_from)
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
		let mut persisted = agent.snapshot().messages.len();
		let cut = auto_compaction_cut(&agent, &session, persisted, 20_000).expect("safe boundary");
		assert_eq!(cut.0, 1);
		let outcome = compaction::compact_prefix(&agent, Some(cut)).await.unwrap();
		let bus = EventBus::new();
		apply_compaction(&agent, &mut session, &mut persisted, &bus, outcome);
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
		let mut session =
			SessionManager::with_file(dir.clone(), std::fs::File::open(path).unwrap());
		let agent = Agent::new(AgentOptions {
			initial_messages: vec![Message::user("original")],
			..Default::default()
		});
		let before = agent.snapshot().messages;
		let mut persisted = before.len();
		apply_compaction(
			&agent,
			&mut session,
			&mut persisted,
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
		let mut persisted = before.len();
		session.fail_next_sync();
		assert!(!apply_compaction(
			&agent,
			&mut session,
			&mut persisted,
			&EventBus::new(),
			outcome
		));
		assert_eq!(agent.snapshot().messages, before);
		assert_eq!(persisted, before.len());
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
		let projection = read_session_projection(&dir).0;
		let mut entry = SessionEntry::compaction("summary", 100);
		if let SessionEntry::Compaction {
			first_kept_entry_id,
			..
		} = &mut entry
		{
			*first_kept_entry_id = projection[1].0.clone().unwrap();
		}
		session.append(&entry).unwrap();
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
	fn missing_cut_id_does_not_discard_history() {
		let dir = temp_session_dir("missing-cut");
		let mut session = SessionManager::open(&dir).unwrap();
		session
			.append_message(&Message::user("safe history"))
			.unwrap();
		let mut entry = SessionEntry::compaction("summary", 1);
		if let SessionEntry::Compaction {
			first_kept_entry_id,
			..
		} = &mut entry
		{
			*first_kept_entry_id = "missing".into();
		}
		session.append(&entry).unwrap();
		let restored = read_session_messages(&dir);
		assert_eq!(restored.len(), 1);
		assert!(
			matches!(&restored[0], Message::User(user) if user.content.as_text() == "safe history")
		);
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
		let mut session = SessionManager::open(&dir).unwrap();
		let agent = mock_agent();
		let mut persisted = agent.snapshot().messages.len();
		agent
			.prompt(PromptInput::Text {
				text: "hello".into(),
			})
			.await
			.unwrap();
		let bus = EventBus::new();
		persist_messages(&agent, &mut session, &mut persisted, &bus);
		persist_messages(&agent, &mut session, &mut persisted, &bus);
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
			SessionManager::open(&dir).unwrap(),
			registry,
			rx,
			bus,
			Arc::new(std::sync::atomic::AtomicBool::new(true)),
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
