//! Agent session — port of
//! `pi/packages/coding-agent/src/core/agent-session.ts` (reduced to the
//! framework-stage surface).
//!
//! `AgentSession` is the mode-agnostic core shared by interactive/print/rpc:
//! it owns the [`Agent`], forwards agent events to subscribers, persists
//! messages to the session log, and serializes UI commands into agent calls
//! (prompt/steer/follow-up/abort/model/thinking). Compaction, bash
//! passthrough, and session switch/fork land in later phases.

use std::collections::VecDeque;
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
	Exit,
}

/// Events the session emits to modes.
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

	SessionHandle {
		cmd_tx,
		events: event_bus.subscribe(),
		agent,
		session_dir,
	}
}

async fn session_task(
	agent: Agent,
	mut session: SessionManager,
	registry: Arc<std::sync::Mutex<ModelRegistry>>,
	mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
	event_bus: EventBus<SessionEvent>,
) {
	let mut agent_rx = agent.subscribe();
	let mut persisted = agent.snapshot().messages.len();
	let mut pending = VecDeque::new();
	let mut run: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;

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
						if run.is_some() || agent.is_streaming() {
							pending.push_back(text);
							notice(&event_bus, "run active — prompt queued".into());
						} else {
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
						notice(&event_bus, "/compact lands with the compaction phase (core/compaction)".into());
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
				if let Some(text) = pending.pop_front() {
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
	persist_messages(&agent, &mut session, &mut persisted, &event_bus);
}

// The broadcast channel is lossy; the transcript is the authoritative source.
// Advance only after a successful append so a later event can retry a failed write.
fn persist_messages(
	agent: &Agent,
	session: &mut SessionManager,
	persisted: &mut usize,
	bus: &EventBus<SessionEvent>,
) {
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
	let Ok(text) = std::fs::read_to_string(dir.join("session.jsonl")) else {
		return Vec::new();
	};
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

/// Messages to seed a resumed agent (`--continue` / `--resume`): the persisted
/// transcript minus system messages. The persist cursor starts past the leading
/// system message, so the log never contains one; on resume [`Agent::new`]
/// (pi-agent) seeds a fresh system message from the current environment —
/// cwd, tools, freshly built prompt — mirroring the upstream
/// `_rebuildSystemPrompt` behaviour in `core/agent-session.ts`.
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
