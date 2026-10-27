//! Agent session — port of
//! `pi/packages/coding-agent/src/core/agent-session.ts` (reduced to the
//! framework-stage surface).
//!
//! `AgentSession` is the mode-agnostic core shared by interactive/print/rpc:
//! it owns the [`Agent`], forwards agent events to subscribers, persists
//! messages to the session log, and serializes UI commands into agent calls
//! (prompt/steer/follow-up/abort/model/thinking). Compaction, bash
//! passthrough, and session switch/fork land in later phases.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use pi_agent::{Agent, AgentEvent, PromptInput};
use pi_ai::{Message, ThinkingLevel};

use super::event_bus::EventBus;
use super::model_registry::ModelRegistry;
use super::session_manager::{SessionEntry, SessionManager};

/// Commands a mode sends to the session task.
#[derive(Debug, Clone)]
pub enum SessionCommand {
	/// New prompt; demoted to steering when a run is active.
	Prompt { text: String },
	/// Inject mid-run after the current assistant turn.
	Steer { text: String },
	/// Queue to run when the agent would otherwise stop.
	FollowUp { text: String },
	Abort,
	SetModel { query: String },
	SetThinking { level: ThinkingLevel },
	CycleThinking,
	Compact,
	Exit,
}

/// Events the session emits to modes.
#[derive(Debug, Clone)]
pub enum SessionEvent {
	Agent(AgentEvent),
	/// Human-facing status/notices (model switched, errors, stub feedback).
	Notice(String),
	/// Persisted a session entry (for `/export` etc.).
	Persisted { kind: &'static str },
}

/// Handle held by modes; the session task runs in the background.
pub struct SessionHandle {
	pub cmd_tx: mpsc::UnboundedSender<SessionCommand>,
	pub events: tokio::sync::broadcast::Receiver<SessionEvent>,
	/// Shared agent for status probes (snapshot, is_streaming, abort).
	pub agent: Agent,
	pub session_dir: Option<std::path::PathBuf>,
}

/// Build and spawn the session task.
pub fn spawn_agent_session(
	agent: Agent,
	session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
) -> SessionHandle {
	let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
	let event_bus = EventBus::<SessionEvent>::new();

	let session_dir = session.session_dir().map(|dir| dir.to_path_buf());
	agent.set_session_id(session.session_id());

	tokio::spawn(session_task(
		agent.clone(),
		session,
		registry,
		cmd_rx,
		event_bus.clone(),
	));

	SessionHandle { cmd_tx, events: event_bus.subscribe(), agent, session_dir }
}

async fn session_task(
	agent: Agent,
	mut session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
	event_bus: EventBus<SessionEvent>,
) {
	let mut agent_rx = agent.subscribe();

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
						if agent.is_streaming() {
							agent.steer(text.clone());
							notice(&event_bus, "run active — message queued as steering".into());
						} else {
							let agent = agent.clone();
							let bus = event_bus.clone();
							tokio::spawn(async move {
								if let Err(e) = agent.prompt(PromptInput::Text { text }).await {
									bus.emit(SessionEvent::Notice(format!("prompt failed: {e}")));
								}
							});
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
						agent.abort();
						notice(&event_bus, "aborting…".into());
					}
					SessionCommand::SetModel { query } => {
						let found = registry.lock().unwrap().find(&query);
						match found {
							Some(model) => {
								agent.set_stream_fn(pi_ai::providers::stream_fn_for(&model));
								agent.set_model(model.clone());
								registry.lock().unwrap().set_default(&model);
								let _ = session.append(&SessionEntry::ModelChange { model: model.key() });
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
						notice(&event_bus, "/compact lands with the compaction phase (core/compaction)".into());
					}
					SessionCommand::Exit => break,
				}
			}

			event = agent_rx.recv() => {
				match event {
					Ok(agent_event) => {
						if let AgentEvent::MessageEnd { message } = &agent_event {
							let _ = session.append_message(message);
							event_bus.emit(SessionEvent::Persisted { kind: "message" });
						}
						event_bus.emit(SessionEvent::Agent(agent_event));
					}
					Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
						notice(&event_bus, format!("event lag: dropped {count} events"));
					}
					Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
				}
			}
		}
	}
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

/// Rebuild a transcript from a session file (resume support, reduced).
pub fn read_session_messages(dir: &std::path::Path) -> Vec<Message> {
	let Ok(text) = std::fs::read_to_string(dir.join("session.jsonl")) else { return Vec::new() };
	text.lines()
		.filter_map(|line| {
			let value: Value = serde_json::from_str(line).ok()?;
			if value.get("type")?.as_str()? == "message" {
				serde_json::from_value(value.get("message")?.clone()).ok()
			} else {
				None
			}
		})
		.collect()
}
