//! RPC mode — port of `pi/packages/coding-agent/src/modes/rpc/rpc-mode.ts`
//! and `rpc-types.ts`.
//!
//! Headless JSONL protocol: commands arrive on stdin as one JSON object per
//! line (`{"type": "...", "id"?: "...", ...}`); responses and events go out on
//! stdout as JSON lines. `id` is echoed on the response when present. LF-only
//! framing, matching `modes/rpc/jsonl.ts`.
//!
//! Wire shape follows the upstream `RpcCommand`/`RpcResponse` types
//! (camelCase fields, `type: "response"` + `command` + `success` +
//! `data`/`error`). Deviations from the upstream wire:
//!
//! - Command subset: `prompt`, `steer`, `follow_up`, `abort`, `set_model`,
//!   `set_thinking_level`, `get_state`, `get_messages`,
//!   `get_available_models`, `get_available_thinking_levels`. Everything else
//!   (bash, compaction, session tree/fork/clone, extension UI, retry controls)
//!   answers with an error frame until the corresponding phases land.
//! - `prompt` answers immediately with `{disposition: "started" | "queued"}`,
//!   inferred from the agent state at enqueue time; upstream reports the
//!   disposition from prompt preflight. `steer`/`follow_up` always answer
//!   `{disposition: "queued"}` (this port has no extension input handlers).
//! - Event frames are the serde serialization of `AgentEvent`
//!   (`{"type": "message_start", "message": {...}}`, camelCase);
//!   `message_update` carries the cumulative assistant message instead of
//!   upstream's stripped delta shape. Session notices — which upstream routes
//!   through extensions — are emitted as `{"type": "notice", "message": ...}`.
//!   `Persisted` bookkeeping events are not forwarded.
//! - `set_model` takes upstream's `provider` + `modelId`; the pair is resolved
//!   through this port's registry query matcher (`provider/id`, id, or fuzzy).
//! - No stdout backpressure handling and no signal handling (upstream waits on
//!   raw-stdout backpressure and traps SIGTERM/SIGHUP).

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use pi_ai::{Model, ThinkingLevel};

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle};
use crate::core::model_registry::ModelRegistry;

/// One command frame from the client — `RpcCommand` in
/// `modes/rpc/rpc-types.ts`, reduced to the commands this port serves.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
	Prompt {
		message: String,
	},
	Steer {
		message: String,
	},
	FollowUp {
		message: String,
	},
	Abort,
	SetModel {
		provider: String,
		#[serde(rename = "modelId")]
		model_id: String,
	},
	SetThinkingLevel {
		level: ThinkingLevel,
	},
	GetState,
	GetMessages,
	GetAvailableModels,
	GetAvailableThinkingLevels,
}

/// Command half of the RPC surface. Pure: parses a frame, translates it into
/// [`SessionCommand`]s, and produces the response frames to write. stdio
/// (`run_rpc`) and tests drive it the same way; events come back through
/// [`RpcSession::next_event_frame`].
pub struct RpcSession {
	cmd_tx: mpsc::UnboundedSender<SessionCommand>,
	agent: pi_agent::Agent,
	session_dir: Option<std::path::PathBuf>,
	registry: Arc<Mutex<ModelRegistry>>,
	events: broadcast::Receiver<SessionEvent>,
}

impl RpcSession {
	pub fn new(handle: SessionHandle, registry: Arc<Mutex<ModelRegistry>>) -> Self {
		let SessionHandle {
			cmd_tx,
			events,
			agent,
			session_dir,
		} = handle;
		Self {
			cmd_tx,
			agent,
			session_dir,
			registry,
			events,
		}
	}

	/// Parse and dispatch one input line; returns the response frames to
	/// write (empty for blank lines, exactly one otherwise).
	pub fn handle_line(&self, line: &str) -> Vec<String> {
		let trimmed = line.trim();
		if trimmed.is_empty() {
			return Vec::new();
		}
		let raw: Value = match serde_json::from_str(trimmed) {
			Ok(value) => value,
			Err(e) => {
				return vec![error_frame(
					&None,
					"parse",
					format!("Failed to parse command: {e}"),
				)];
			}
		};
		// `id` is an opaque correlation token (upstream types it `string` but
		// passes it through untouched); echo whatever the client sent.
		let id = raw.get("id").cloned().filter(|value| !value.is_null());
		let command_type = raw
			.get("type")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_string();
		let command: RpcCommand = match serde_json::from_value(raw) {
			Ok(command) => command,
			Err(e) => {
				let name = if command_type.is_empty() {
					"unknown"
				} else {
					&command_type
				};
				return vec![error_frame(&id, name, format!("invalid command: {e}"))];
			}
		};
		vec![self.dispatch(command, id)]
	}

	/// Next outbound frame from the session (agent events as JSON, notices as
	/// `{"type":"notice"}`); `None` once the session task has shut down.
	pub async fn next_event_frame(&mut self) -> Option<String> {
		loop {
			match self.events.recv().await {
				Ok(SessionEvent::Agent(event)) => {
					return Some(serde_json::to_string(&*event).expect("agent events serialize"));
				}
				Ok(SessionEvent::Notice(text)) => {
					return Some(json!({"type": "notice", "message": text}).to_string());
				}
				// Internal bookkeeping; not part of the wire protocol.
				Ok(SessionEvent::Persisted { .. }) => continue,
				Err(broadcast::error::RecvError::Lagged(count)) => {
					return Some(
						json!({"type": "notice", "message": format!("event lag: dropped {count} events")})
							.to_string(),
					);
				}
				Err(broadcast::error::RecvError::Closed) => return None,
			}
		}
	}

	/// Shut the session task down (persisting any transcript tail).
	pub fn shutdown(&self) {
		let _ = self.cmd_tx.send(SessionCommand::Exit);
	}

	/// stdin-EOF shutdown: abort an active run, wait for it to settle so the
	/// transcript tail is persisted, then stop the session task (upstream
	/// `shutdown()` → `runtimeHost.dispose()` in `rpc-mode.ts`).
	pub async fn shutdown_settled(&mut self) {
		self.shutdown();
		while self.next_event_frame().await.is_some() {}
	}

	fn dispatch(&self, command: RpcCommand, id: Option<Value>) -> String {
		match command {
			RpcCommand::Prompt { message } => {
				// Mirrors the session task's queue rule; a queued prompt is
				// still a success (upstream "queued" vs "started").
				let disposition = if self.is_run_active() {
					"queued"
				} else {
					"started"
				};
				let _ = self.cmd_tx.send(SessionCommand::Prompt { text: message });
				success_frame(&id, "prompt", json!({ "disposition": disposition }))
			}
			RpcCommand::Steer { message } => {
				let _ = self.cmd_tx.send(SessionCommand::Steer { text: message });
				success_frame(&id, "steer", json!({ "disposition": "queued" }))
			}
			RpcCommand::FollowUp { message } => {
				let _ = self.cmd_tx.send(SessionCommand::FollowUp { text: message });
				success_frame(&id, "follow_up", json!({ "disposition": "queued" }))
			}
			RpcCommand::Abort => {
				let _ = self.cmd_tx.send(SessionCommand::Abort);
				success_frame(&id, "abort", Value::Null)
			}
			RpcCommand::SetModel { provider, model_id } => {
				let query = format!("{provider}/{model_id}");
				let found = self.registry.lock().unwrap().find(&query);
				match found {
					Some(model) => {
						let data = serde_json::to_value(&model).unwrap_or(Value::Null);
						let _ = self.cmd_tx.send(SessionCommand::SetModel { query });
						success_frame(&id, "set_model", data)
					}
					None => error_frame(&id, "set_model", format!("Model not found: {query}")),
				}
			}
			RpcCommand::SetThinkingLevel { level } => {
				let _ = self.cmd_tx.send(SessionCommand::SetThinking { level });
				success_frame(&id, "set_thinking_level", Value::Null)
			}
			RpcCommand::GetState => success_frame(&id, "get_state", self.state_value()),
			RpcCommand::GetMessages => {
				let messages = self.agent.snapshot().messages;
				success_frame(&id, "get_messages", json!({ "messages": messages }))
			}
			RpcCommand::GetAvailableModels => {
				let models: Vec<Model> = self.registry.lock().unwrap().models().to_vec();
				success_frame(&id, "get_available_models", json!({ "models": models }))
			}
			RpcCommand::GetAvailableThinkingLevels => success_frame(
				&id,
				"get_available_thinking_levels",
				json!({ "levels": all_thinking_levels() }),
			),
		}
	}

	fn is_run_active(&self) -> bool {
		self.agent.is_streaming()
	}

	fn state_value(&self) -> Value {
		let snapshot = self.agent.snapshot();
		json!({
			"model": snapshot.model,
			"thinkingLevel": snapshot.thinking_level,
			"isStreaming": self.agent.is_streaming(),
			"sessionFile": self
				.session_dir
				.as_ref()
				.map(|dir| dir.join("session.jsonl").display().to_string()),
			"sessionId": self.agent.session_id(),
			"messageCount": snapshot.messages.len(),
			"pendingMessageCount": snapshot.queued_steering + snapshot.queued_follow_up,
		})
	}
}

fn all_thinking_levels() -> [&'static str; 7] {
	["off", "minimal", "low", "medium", "high", "xhigh", "max"]
}

fn success_frame(id: &Option<Value>, command: &str, data: Value) -> String {
	let mut frame = json!({
		"type": "response",
		"command": command,
		"success": true,
	});
	if let Some(id) = id {
		frame["id"] = json!(id);
	}
	if !data.is_null() {
		frame["data"] = data;
	}
	frame.to_string()
}

fn error_frame(id: &Option<Value>, command: &str, message: impl std::fmt::Display) -> String {
	let mut frame = json!({
		"type": "response",
		"command": command,
		"success": false,
		"error": message.to_string(),
	});
	if let Some(id) = id {
		frame["id"] = id.clone();
	}
	frame.to_string()
}

/// stdio driver: stdin lines → [`RpcSession::handle_line`], session events →
/// stdout frames. EOF on stdin shuts the session down (persisting any tail).
///
/// stdin is read on a blocking thread (strict LF framing, no readline-style
/// Unicode separator splitting, mirroring `modes/rpc/jsonl.ts`).
pub async fn run_rpc(
	handle: SessionHandle,
	registry: Arc<Mutex<ModelRegistry>>,
) -> anyhow::Result<()> {
	let mut session = RpcSession::new(handle, registry);
	let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
	std::thread::spawn(move || {
		let stdin = std::io::stdin();
		let mut line = String::new();
		let mut reader = stdin.lock();
		loop {
			line.clear();
			match reader.read_line(&mut line) {
				Ok(0) | Err(_) => break,
				Ok(_) => {
					if line_tx.send(line.clone()).is_err() {
						break;
					}
				}
			}
		}
	});

	let stdout = std::io::stdout();
	let mut out = stdout.lock();
	macro_rules! emit {
		($frame:expr) => {{
			writeln!(out, "{}", $frame)?;
			out.flush()?;
		}};
	}
	loop {
		tokio::select! {
			line = line_rx.recv() => {
				let Some(line) = line else { break };
				for frame in session.handle_line(&line) {
					emit!(frame);
				}
			}
			frame = session.next_event_frame() => {
				let Some(frame) = frame else { break };
				emit!(frame);
			}
		}
	}
	session.shutdown_settled().await;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_agent::{Agent, AgentOptions};
	use std::time::Duration;

	fn mock_handle() -> (SessionHandle, Arc<Mutex<ModelRegistry>>) {
		let stream: pi_ai::StreamFn = Arc::new(
			|model: &pi_ai::Model,
			 context: &pi_ai::TranscriptContext,
			 options: pi_ai::StreamOptions| {
				pi_ai::providers::mock::stream(model, context, options)
			},
		);
		let agent = Agent::new(AgentOptions {
			model: Model::new("mock-echo", "Mock", "mock", "mock", ""),
			stream_fn: Some(stream),
			..AgentOptions::default()
		});
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
		);
		(handle, registry)
	}

	fn parse_frame(frame: &str) -> Value {
		serde_json::from_str(frame).expect("frames are JSON")
	}

	async fn until_agent_end(session: &mut RpcSession) -> Vec<Value> {
		tokio::time::timeout(Duration::from_secs(5), async {
			let mut frames = Vec::new();
			loop {
				let frame = session
					.next_event_frame()
					.await
					.expect("session stays open until agent_end");
				let value = parse_frame(&frame);
				let done = value["type"] == "agent_end";
				frames.push(value);
				if done {
					return frames;
				}
			}
		})
		.await
		.expect("run should finish in time")
	}

	#[tokio::test]
	async fn blank_lines_produce_no_frames() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		assert!(session.handle_line("").is_empty());
		assert!(session.handle_line("   \t ").is_empty());
	}

	#[tokio::test]
	async fn invalid_json_yields_parse_error_frame() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session.handle_line("{not json");
		assert_eq!(frames.len(), 1);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["type"], "response");
		assert_eq!(frame["command"], "parse");
		assert_eq!(frame["success"], false);
		assert!(frame["error"].as_str().unwrap().contains("Failed to parse"));
	}

	#[tokio::test]
	async fn unknown_command_yields_error_frame() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"fork","id":"7"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], false);
		assert_eq!(frame["id"], "7");
		assert!(frame["error"].as_str().unwrap().contains("fork"));
	}

	#[tokio::test]
	async fn ids_are_echoed_opaquely() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		// Upstream types `id` as string but passes it through untouched;
		// non-string ids are echoed verbatim instead of rejected.
		let frames = session.handle_line(r#"{"type":"get_state","id":42}"#);
		assert_eq!(parse_frame(&frames[0])["id"], 42);
	}

	#[tokio::test]
	async fn command_with_missing_fields_is_an_error_not_a_panic() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"prompt","id":"9"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], false);
		assert_eq!(frame["command"], "prompt");
		assert_eq!(frame["id"], "9");
	}

	#[tokio::test]
	async fn get_state_reports_session_and_agent_fields() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"get_state","id":"s1"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["type"], "response");
		assert_eq!(frame["command"], "get_state");
		assert_eq!(frame["success"], true);
		assert_eq!(frame["id"], "s1");
		let data = &frame["data"];
		assert_eq!(data["isStreaming"], false);
		assert_eq!(data["thinkingLevel"], "off");
		assert_eq!(data["messageCount"], 1); // leading system message
		assert_eq!(data["pendingMessageCount"], 0);
		assert!(data["model"]["provider"].is_string());
	}

	#[tokio::test]
	async fn set_thinking_level_roundtrips() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"set_thinking_level","level":"high"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], true);
		assert_eq!(frame["command"], "set_thinking_level");
		// Applied by the session task; the notice confirms it landed.
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = session.next_event_frame().await.unwrap();
				if frame.contains("thinking") {
					break;
				}
			}
		})
		.await
		.expect("thinking notice should arrive in time");
		assert_eq!(session.agent.thinking_level(), ThinkingLevel::High);

		let frames = session.handle_line(r#"{"type":"set_thinking_level","level":"bogus"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], false);
	}

	#[tokio::test]
	async fn set_model_resolves_and_rejects() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames =
			session.handle_line(r#"{"type":"set_model","provider":"mock","modelId":"mock-echo"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["id"], "mock-echo");
		assert_eq!(frame["data"]["provider"], "mock");

		let frames = session
			.handle_line(r#"{"type":"set_model","provider":"nosuch","modelId":"model","id":"m1"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], false);
		assert_eq!(frame["id"], "m1");
		assert!(frame["error"].as_str().unwrap().contains("Model not found"));
	}

	#[tokio::test]
	async fn get_available_models_and_thinking_levels() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_available_models"}"#)[0]);
		assert_eq!(frame["success"], true);
		assert!(!frame["data"]["models"].as_array().unwrap().is_empty());

		let frame =
			parse_frame(&session.handle_line(r#"{"type":"get_available_thinking_levels"}"#)[0]);
		assert_eq!(
			frame["data"]["levels"],
			json!(["off", "minimal", "low", "medium", "high", "xhigh", "max"])
		);
	}

	#[tokio::test]
	async fn get_messages_lists_transcript() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_messages"}"#)[0]);
		assert_eq!(frame["data"]["messages"][0]["role"], "system");
	}

	#[tokio::test]
	async fn prompt_roundtrip_streams_events_to_agent_end() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"prompt","message":"hello","id":"p1"}"#);
		let response = parse_frame(&frames[0]);
		assert_eq!(response["command"], "prompt");
		assert_eq!(response["success"], true);
		assert_eq!(response["id"], "p1");
		assert_eq!(response["data"]["disposition"], "started");

		let events = until_agent_end(&mut session).await;
		let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
		assert!(kinds.contains(&"agent_start"));
		assert!(kinds.contains(&"message_start"));
		assert!(kinds.contains(&"message_end"));
		assert_eq!(*kinds.last().unwrap(), "agent_end");
		// The final assistant text echoes the prompt (mock provider).
		let message_end = events
			.iter()
			.rev()
			.find(|e| e["type"] == "message_end")
			.unwrap();
		let text = message_end["message"]["content"][0]["text"]
			.as_str()
			.unwrap();
		assert!(text.contains("hello"));
	}

	#[tokio::test]
	async fn prompt_while_streaming_reports_queued_and_runs_later() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"first"}"#);
		// Wait until the run is streaming so the disposition is deterministic.
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = session.next_event_frame().await.unwrap();
				if frame.contains("message_start") {
					break;
				}
			}
		})
		.await
		.expect("run should start in time");
		let frames = session.handle_line(r#"{"type":"prompt","message":"second"}"#);
		assert_eq!(parse_frame(&frames[0])["data"]["disposition"], "queued");
		until_agent_end(&mut session).await;
		until_agent_end(&mut session).await;
		let users: Vec<String> = session
			.agent
			.snapshot()
			.messages
			.iter()
			.filter_map(|m| match m {
				pi_ai::Message::User(u) => Some(u.content.as_text().to_string()),
				_ => None,
			})
			.collect();
		assert_eq!(users, ["first", "second"]);
	}

	#[tokio::test]
	async fn steer_during_run_becomes_a_turn() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"prompt","message":"hello"}"#);
		assert_eq!(parse_frame(&frames[0])["data"]["disposition"], "started");
		// Wait until the run is actually streaming before steering.
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = session.next_event_frame().await.unwrap();
				if frame.contains("message_start") {
					break;
				}
			}
		})
		.await
		.expect("run should start in time");

		let frames = session.handle_line(r#"{"type":"steer","message":"steer now"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["command"], "steer");
		assert_eq!(frame["data"]["disposition"], "queued");
		until_agent_end(&mut session).await;

		let users: Vec<String> = session
			.agent
			.snapshot()
			.messages
			.iter()
			.filter_map(|m| match m {
				pi_ai::Message::User(u) => Some(u.content.as_text().to_string()),
				_ => None,
			})
			.collect();
		assert_eq!(users, ["hello", "steer now"]);
	}

	#[tokio::test]
	async fn abort_answers_and_stops_the_stream() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"hello"}"#);
		let frames = session.handle_line(r#"{"type":"abort","id":"a1"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["command"], "abort");
		assert_eq!(frame["success"], true);
		assert_eq!(frame["id"], "a1");
		// A run may already have finished on a fast machine; either way the
		// session settles and the transcript stays coherent.
		let _ = until_agent_end(&mut session).await;
		assert!(!session.agent.is_streaming());
	}

	#[tokio::test]
	async fn follow_up_answers_queued() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"follow_up","message":"later"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["command"], "follow_up");
		assert_eq!(frame["data"]["disposition"], "queued");
	}
}
