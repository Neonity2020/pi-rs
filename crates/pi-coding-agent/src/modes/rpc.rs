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
//! - Command subset (wire names follow upstream `rpc-types.ts`): served —
//!   `prompt`, `steer`, `follow_up`, `abort`, `clear_queue`, `set_model`,
//!   `cycle_model`, `set_thinking_level`, `cycle_thinking_level`, `get_state`,
//!   `get_messages`, `get_available_models`, `get_available_thinking_levels`,
//!   `set_steering_mode`, `set_follow_up_mode`, `compact`,
//!   `set_auto_compaction`, `get_last_assistant_text`, `get_commands`,
//!   `new_session`, `switch_session`, `fork`, `clone`,
//!   `get_fork_messages`, `get_entries`, `get_tree`, `get_session_stats`,
//!   `set_session_name`, `bash`, `abort_bash`, `set_auto_retry`,
//!   `abort_retry`. Deferred until their phases land, and answered with an
//!   error frame meanwhile: `export_html` (wired by the integrating session),
//!   `prompt`'s images/streamingBehavior, and the extension UI bridge
//!   (`extension_ui_request`/`extension_ui_response`).
//! - `prompt` answers immediately with `{disposition: "started" | "queued"}`,
//!   inferred from the agent state at enqueue time; upstream reports the
//!   disposition from prompt preflight. `steer`/`follow_up` always answer
//!   `{disposition: "queued"}` (this port has no extension input handlers).
//!   `compact` answers with the same disposition inference instead of the
//!   upstream `CompactionResult` — the session task reports compaction
//!   progress through notices.
//! - `clear_queue` returns the drained texts split per queue
//!   (`{steering, followUp}`), peeked and cleared through pi-agent's
//!   per-queue accessors (`peek_steering_queue`/`peek_follow_up_queue` +
//!   `clear_*_queue`), like upstream `session.clearQueue()`. One divergence:
//!   upstream reports every queued text, while pi-agent's peek reports what
//!   the current queue mode would deliver next (`one-at-a-time` peeks only
//!   the oldest message per queue); everything queued is cleared either way.
//! - Session replacement (`new_session`/`switch_session`/`fork`/`clone`):
//!   the replacement happens asynchronously in the session task, so the
//!   response frames answer `{cancelled: false}` immediately (extensions can
//!   cancel upstream — this port has none, hence always false).
//!   `switch_session` pre-validates the target (valid log, existing cwd —
//!   upstream `assertSessionCwdExists`) so failures answer synchronously;
//!   `fork` pre-validates the entry (must be a user message for the default
//!   `before` position) and computes `selectedText` for the response. After a
//!   replacement, `get_state` and friends read the live session through the
//!   session task's shared slots.
//! - `bash` answers asynchronously: the response frame carries the finished
//!   `BashResult` and is emitted when the execution completes; streaming
//!   output arrives as `bash_execution_update` frames meanwhile. Upstream
//!   awaits `session.executeBash` inside the command handler, which delays
//!   event frames for the same duration.
//! - `get_state` mirrors the upstream 12-field `RpcSessionState`;
//!   `isCompacting` mirrors the session task's in-flight compaction flag, and
//!   `steeringMode`/`followUpMode` come from the agent's queue modes.
//!   `cycle_model` reuses the `set_model` response shape (the model object)
//!   instead of upstream's `{model, thinkingLevel, isScoped}`.
//! - `agent_end` frames carry the upstream `willRetry` field; the session
//!   task ships the per-run decision alongside the event. `agent_settled` is
//!   the bare `{"type": "agent_settled"}` frame — upstream carries an
//!   `aborted` field, which the session layer does not track; the settle
//!   timing (only when no run, compaction, or queued prompt remains — i.e.
//!   no auto-continuation pending) matches upstream.
//! - Thinking-level commands are trimmed to the model's capabilities:
//!   `get_available_thinking_levels` returns `["off"]` for models without
//!   reasoning support, and `cycle_thinking_level` answers with a null data
//!   field for those models (upstream `cycleThinkingLevel` → undefined).
//!   Per-model `thinkingLevelMap` filtering is not modeled — the Rust `Model`
//!   has no level map, so reasoning models report all seven levels.
//! - Event frame field names follow the upstream camelCase convention.
//!   Session notices — which upstream routes
//!   through extensions — are emitted as `{"type": "notice", "message": ...}`.
//!   `Persisted` bookkeeping events are not forwarded.
//! - `set_model` matches the upstream's exact `provider` + `modelId` pair.
//! - No stdout backpressure handling and no signal handling (upstream waits on
//!   raw-stdout backpressure and traps SIGTERM/SIGHUP).

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use pi_ai::{Model, ThinkingLevel};

use crate::core::agent_session::{
	ForkPosition, SessionCommand, SessionEvent, SessionHandle, WireEvent,
};
use crate::core::model_registry::ModelRegistry;
use crate::core::session_manager::{EntryKind, SessionEntry, SessionTreeNode, peek_session_header};

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
	ClearQueue,
	NewSession {
		#[serde(rename = "parentSession")]
		parent_session: Option<String>,
	},
	SetModel {
		provider: String,
		#[serde(rename = "modelId")]
		model_id: String,
	},
	CycleModel,
	SetThinkingLevel {
		level: ThinkingLevel,
	},
	CycleThinkingLevel,
	GetState,
	GetAvailableModels,
	GetAvailableThinkingLevels,
	SetSteeringMode {
		mode: RpcQueueMode,
	},
	SetFollowUpMode {
		mode: RpcQueueMode,
	},
	Compact,
	SetAutoCompaction {
		enabled: bool,
	},
	SetAutoRetry {
		enabled: bool,
	},
	AbortRetry,
	Bash {
		command: String,
		#[serde(rename = "excludeFromContext")]
		exclude_from_context: Option<bool>,
	},
	AbortBash,
	GetSessionStats,
	SwitchSession {
		#[serde(rename = "sessionPath")]
		session_path: String,
	},
	Fork {
		#[serde(rename = "entryId")]
		entry_id: String,
	},
	Clone,
	GetForkMessages,
	GetEntries {
		since: Option<String>,
	},
	GetTree,
	GetLastAssistantText,
	SetSessionName {
		name: String,
	},
	GetMessages,
	GetCommands,
	ExportHtml {
		#[serde(rename = "outputPath")]
		output_path: Option<String>,
	},
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RpcQueueMode {
	All,
	OneAtATime,
}

impl From<RpcQueueMode> for pi_agent::QueueMode {
	fn from(mode: RpcQueueMode) -> Self {
		match mode {
			RpcQueueMode::All => Self::All,
			RpcQueueMode::OneAtATime => Self::OneAtATime,
		}
	}
}

/// Command half of the RPC surface. Pure: parses a frame, translates it into
/// [`SessionCommand`]s, and produces the response frames to write. stdio
/// (`run_rpc`) and tests drive it the same way; events come back through
/// [`RpcSession::next_event_frame`].
pub struct RpcSession {
	cmd_tx: mpsc::UnboundedSender<SessionCommand>,
	/// The live agent, read through the session task's shared slot so probes
	/// survive a session replacement (`new_session`/`switch_session`/fork).
	live_agent: Arc<Mutex<pi_agent::Agent>>,
	/// The live session log — entries, tree, leaf, and name probes.
	log: crate::core::agent_session::SharedLogRef,
	/// Live-session slots (current file, display name) and the wire-event bus.
	shared: Arc<crate::core::agent_session::SessionShared>,
	registry: Arc<Mutex<ModelRegistry>>,
	events: broadcast::Receiver<SessionEvent>,
	wire_events: broadcast::Receiver<WireEvent>,
	/// Mirrors the session task's auto-compaction toggle (lock-free probe for
	/// `get_state`; the session task owns the authoritative state).
	auto_compaction: Arc<std::sync::atomic::AtomicBool>,
	/// True while a compaction summarization is in flight (`get_state`'s
	/// `isCompacting`; the session task owns the authoritative state).
	compacting: Arc<std::sync::atomic::AtomicBool>,
}

impl RpcSession {
	pub fn new(handle: SessionHandle, registry: Arc<Mutex<ModelRegistry>>) -> Self {
		let SessionHandle {
			cmd_tx,
			events,
			agent,
			session_dir: _,
			auto_compaction,
			compacting,
			log,
			shared,
			wire_events,
			auto_retry: _,
		} = handle;
		let live_agent = shared.live_agent.clone();
		*live_agent.lock().unwrap() = agent;
		Self {
			cmd_tx,
			live_agent,
			log,
			shared,
			registry,
			events,
			wire_events,
			auto_compaction,
			compacting,
		}
	}

	/// The agent to probe: the session task's live instance, so a session
	/// replacement is observable without re-binding.
	fn agent(&self) -> pi_agent::Agent {
		self.live_agent.lock().unwrap().clone()
	}

	/// Parse and dispatch one input line; returns the response frames to
	/// write (empty for blank lines and for `bash`, whose response frame is
	/// emitted when the execution finishes).
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
		// `bash` answers asynchronously: the response carries the finished
		// `BashResult` and is emitted when the session task reports the
		// execution done (upstream awaits inside the handler).
		if let RpcCommand::Bash {
			command,
			exclude_from_context,
		} = command
		{
			let _ = self.cmd_tx.send(SessionCommand::ExecuteBash {
				command,
				exclude_from_context: exclude_from_context.unwrap_or(false),
				id: id.clone(),
			});
			return Vec::new();
		}
		vec![self.dispatch(command, id)]
	}

	/// Next outbound frame from the session (agent events as JSON, notices as
	/// `{"type":"notice"}`, plus the wire-level bash/retry/session-info
	/// events); `None` once the session task has shut down.
	pub async fn next_event_frame(&mut self) -> Option<String> {
		loop {
			tokio::select! {
				event = self.events.recv() => match event {
					Ok(SessionEvent::Agent(event)) => {
						let mut frame = agent_event_frame(&event);
						if matches!(&*event, pi_agent::AgentEvent::AgentEnd { .. }) {
							// The upstream `agent_end` carries `willRetry`; the
							// session task ships one decision per forwarded
							// end, in order.
							let will_retry = self
								.shared
								.agent_end_will_retry
								.lock()
								.unwrap()
								.pop_front()
								.unwrap_or(false);
							frame = match serde_json::from_str::<Value>(&frame) {
								Ok(mut value) => {
									value["willRetry"] = json!(will_retry);
									value.to_string()
								}
								Err(_) => frame,
							};
						}
						return Some(frame);
					}
					Ok(SessionEvent::Notice(text)) => {
						return Some(json!({"type": "notice", "message": text}).to_string());
					}
					Ok(SessionEvent::Settled { aborted }) => {
						return Some(
							json!({"type": "agent_settled", "aborted": aborted}).to_string(),
						);
					}
					// Internal bookkeeping; not part of the wire protocol.
					Ok(SessionEvent::Persisted { .. }) => continue,
					Err(broadcast::error::RecvError::Lagged(count)) => {
						// Dropped events can include an `agent_end`, whose
						// queued willRetry decision is now unpairable — drop
						// the pending queue with them (advisory flag,
						// self-corrects on the next run).
						self.shared.agent_end_will_retry.lock().unwrap().clear();
						return Some(
							json!({"type": "notice", "message": format!("event lag: dropped {count} events")})
								.to_string(),
						);
					}
					Err(broadcast::error::RecvError::Closed) => return None,
				},
				event = self.wire_events.recv() => match event {
					Ok(WireEvent::BashExecutionUpdate { id, delta }) => {
						let mut frame = json!({"type": "bash_execution_update", "delta": delta});
						if let Some(id) = id {
							frame["id"] = id;
						}
						return Some(frame.to_string());
					}
					Ok(WireEvent::BashExecutionDone { id, result }) => {
						return Some(success_frame(&id, "bash", result.to_value()));
					}
					Ok(WireEvent::SessionInfoChanged { name }) => {
						let mut frame = json!({"type": "session_info_changed"});
						if let Some(name) = name {
							frame["name"] = json!(name);
						}
						return Some(frame.to_string());
					}
					Ok(WireEvent::AutoRetryStart { attempt, max_attempts, delay_ms, error_message }) => {
						return Some(json!({
							"type": "auto_retry_start",
							"attempt": attempt,
							"maxAttempts": max_attempts,
							"delayMs": delay_ms,
							"errorMessage": error_message,
						}).to_string());
					}
					Ok(WireEvent::AutoRetryEnd { success, attempt, final_error }) => {
						let mut frame = json!({
							"type": "auto_retry_end",
							"success": success,
							"attempt": attempt,
						});
						if let Some(final_error) = final_error {
							frame["finalError"] = json!(final_error);
						}
						return Some(frame.to_string());
					}
					Err(broadcast::error::RecvError::Lagged(_)) => continue,
					// The wire bus dies with the session handle; the session
					// event channel governs shutdown.
					Err(broadcast::error::RecvError::Closed) => continue,
				},
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
			RpcCommand::ClearQueue => {
				// Upstream `clearQueue()`: snapshot both queues, clear them,
				// report the drained texts split per queue.
				let steering = queued_texts(self.agent().peek_steering_queue());
				let follow_up = queued_texts(self.agent().peek_follow_up_queue());
				self.agent().clear_steering_queue();
				self.agent().clear_follow_up_queue();
				success_frame(
					&id,
					"clear_queue",
					json!({"steering": steering, "followUp": follow_up}),
				)
			}
			RpcCommand::SetModel { provider, model_id } => {
				let query = format!("{provider}/{model_id}");
				let found = self
					.registry
					.lock()
					.unwrap()
					.models()
					.iter()
					.find(|model| model.provider == provider && model.id == model_id)
					.cloned();
				match found {
					Some(model) => {
						let data = serde_json::to_value(&model).unwrap_or(Value::Null);
						let _ = self.cmd_tx.send(SessionCommand::SetModel { query });
						success_frame(&id, "set_model", data)
					}
					None => error_frame(&id, "set_model", format!("Model not found: {query}")),
				}
			}
			RpcCommand::CycleModel => {
				// Next registry model after the current one, wrapping around
				// (upstream `_cycleAvailableModel`); an unknown current model
				// starts the cycle at the first registry entry.
				let current = self.agent().model();
				let next = {
					let registry = self.registry.lock().unwrap();
					let models = registry.models();
					if models.is_empty() {
						None
					} else {
						let index = models
							.iter()
							.position(|m| m.provider == current.provider && m.id == current.id)
							.unwrap_or(0);
						Some(models[(index + 1) % models.len()].clone())
					}
				};
				match next {
					// Same success/failure shape as `set_model`; the session
					// task announces the switch with a `model → …` notice.
					Some(model) => {
						let data = serde_json::to_value(&model).unwrap_or(Value::Null);
						let _ = self
							.cmd_tx
							.send(SessionCommand::SetModel { query: model.key() });
						success_frame(&id, "cycle_model", data)
					}
					None => error_frame(&id, "cycle_model", "no models available to cycle"),
				}
			}
			RpcCommand::SetThinkingLevel { level } => {
				let _ = self.cmd_tx.send(SessionCommand::SetThinking { level });
				success_frame(&id, "set_thinking_level", Value::Null)
			}
			RpcCommand::CycleThinkingLevel => {
				// Upstream `cycleThinkingLevel`: undefined (no change) for
				// models without reasoning support, otherwise the next level
				// of the model's supported ladder, wrapping around.
				if !self.agent().model().reasoning {
					success_frame(&id, "cycle_thinking_level", Value::Null)
				} else {
					let next = next_thinking_level(self.agent().thinking_level());
					let _ = self
						.cmd_tx
						.send(SessionCommand::SetThinking { level: next });
					success_frame(&id, "cycle_thinking_level", json!({ "level": next }))
				}
			}
			RpcCommand::GetState => success_frame(&id, "get_state", self.state_value()),
			RpcCommand::GetMessages => {
				let messages = self.agent().snapshot().messages;
				success_frame(&id, "get_messages", json!({ "messages": messages }))
			}
			RpcCommand::GetAvailableModels => {
				let models: Vec<Model> = self.registry.lock().unwrap().models().to_vec();
				success_frame(&id, "get_available_models", json!({ "models": models }))
			}
			RpcCommand::GetAvailableThinkingLevels => {
				let levels: Vec<&str> = supported_thinking_levels(&self.agent().model())
					.iter()
					.map(|level| thinking_level_wire(*level))
					.collect();
				success_frame(
					&id,
					"get_available_thinking_levels",
					json!({ "levels": levels }),
				)
			}
			RpcCommand::SetSteeringMode { mode } => {
				self.agent().set_steering_mode(mode.into());
				success_frame(&id, "set_steering_mode", Value::Null)
			}
			RpcCommand::SetFollowUpMode { mode } => {
				self.agent().set_follow_up_mode(mode.into());
				success_frame(&id, "set_follow_up_mode", Value::Null)
			}
			RpcCommand::Compact => {
				// Same disposition inference as `prompt`; the session task
				// reports progress/refusals through notices.
				let disposition = if self.is_run_active() {
					"queued"
				} else {
					"started"
				};
				let _ = self.cmd_tx.send(SessionCommand::Compact);
				success_frame(&id, "compact", json!({ "disposition": disposition }))
			}
			RpcCommand::SetAutoCompaction { enabled } => {
				let _ = self
					.cmd_tx
					.send(SessionCommand::SetAutoCompaction { enabled });
				success_frame(&id, "set_auto_compaction", Value::Null)
			}
			RpcCommand::GetLastAssistantText => {
				let snapshot = self.agent().snapshot();
				let last = snapshot
					.messages
					.iter()
					.rev()
					.find_map(|message| match message {
						pi_ai::Message::Assistant(assistant)
							if assistant.stop_reason != pi_ai::StopReason::Aborted
								|| !assistant.content.is_empty() =>
						{
							Some(assistant)
						}
						_ => None,
					});
				let text = last
					.map(|message| message.text())
					.filter(|text| !text.trim().is_empty());
				// Upstream wire: `data: { text: string | null }` — no text is
				// a success with a null field, not an error.
				success_frame(&id, "get_last_assistant_text", json!({ "text": text }))
			}
			RpcCommand::GetCommands => {
				// pi-rs has a single command source: the built-in slash
				// commands of `core/slash_commands.rs`. Upstream also merges
				// extension commands and `skill:` entries (sources
				// "extension"/"skill" with `sourceInfo`); those land with the
				// extension phase, so every command reports source "prompt".
				let commands: Vec<Value> = crate::core::slash_commands::BUILTIN_SLASH_COMMANDS
					.iter()
					.map(|command| {
						json!({
							"name": command.name,
							"description": command.description,
							"source": "prompt",
						})
					})
					.collect();
				success_frame(&id, "get_commands", json!({ "commands": commands }))
			}
			RpcCommand::NewSession { parent_session } => {
				// The replacement runs in the session task; extensions could
				// cancel upstream, and this port has none, so the response is
				// always `cancelled: false` (the module doc records the
				// async-response skew).
				let _ = self
					.cmd_tx
					.send(SessionCommand::NewSession { parent_session });
				success_frame(&id, "new_session", json!({ "cancelled": false }))
			}
			RpcCommand::SwitchSession { session_path } => {
				// Pre-validate synchronously so failures answer like upstream
				// (which awaits the switch): a valid log, and an existing
				// stored cwd (`assertSessionCwdExists`).
				let path = std::path::Path::new(&session_path);
				let dir = if path.is_dir() {
					path
				} else {
					path.parent().unwrap_or(path)
				};
				match peek_session_header(dir) {
					None => error_frame(
						&id,
						"switch_session",
						format!("cannot open session {session_path}: no session header"),
					),
					Some(header)
						if !header.cwd.is_empty()
							&& !std::path::Path::new(&header.cwd).exists() =>
					{
						error_frame(
							&id,
							"switch_session",
							format!(
								"Stored session working directory does not exist: {}",
								header.cwd
							),
						)
					}
					Some(_) => {
						let _ = self
							.cmd_tx
							.send(SessionCommand::SwitchSession { path: session_path });
						success_frame(&id, "switch_session", json!({ "cancelled": false }))
					}
				}
			}
			RpcCommand::Fork { entry_id } => {
				// Upstream `fork` defaults to position "before": the entry
				// must be a user message and the message text rides back as
				// `selectedText`. Pre-computed from the live log so the
				// response matches upstream's awaited result.
				let outcome: Result<Option<String>, String> = {
					let guard = self.log.lock().unwrap();
					let session = &guard.session;
					if session.session_dir().is_none() {
						Err(
							"This session has not been saved yet. Send a message before cloning or forking it."
								.into(),
						)
					} else {
						match session.get_entry(&entry_id) {
							None => Err("Invalid entry ID for forking".into()),
							Some(entry) => match &entry.kind {
								EntryKind::Message { message } => match message.as_ref() {
									pi_ai::Message::User(user) => Ok(Some(user.content.as_text())),
									_ => Err("Invalid entry ID for forking".into()),
								},
								_ => Err("Invalid entry ID for forking".into()),
							},
						}
					}
				};
				match outcome {
					Err(error) => error_frame(&id, "fork", error),
					Ok(selected_text) => {
						let _ = self.cmd_tx.send(SessionCommand::Fork {
							entry_id,
							position: ForkPosition::Before,
						});
						let mut data = json!({ "cancelled": false });
						if let Some(text) = selected_text {
							data["text"] = json!(text);
						}
						success_frame(&id, "fork", data)
					}
				}
			}
			RpcCommand::Clone => {
				// Upstream clones at the current leaf (`fork(leafId, {position: "at"})`).
				let (persisted, leaf) = {
					let guard = self.log.lock().unwrap();
					(
						guard.session.session_dir().is_some(),
						guard.session.leaf_id().map(str::to_owned),
					)
				};
				match (persisted, leaf) {
					(_, None) => error_frame(
						&id,
						"clone",
						"Cannot clone session: no current entry selected",
					),
					// Same unsaved refusal as upstream (runtime.ts:314).
					(false, Some(_)) => error_frame(
						&id,
						"clone",
						"This session has not been saved yet. Send a message before cloning or forking it.",
					),
					(true, Some(entry_id)) => {
						let _ = self.cmd_tx.send(SessionCommand::Fork {
							entry_id,
							position: ForkPosition::At,
						});
						success_frame(&id, "clone", json!({ "cancelled": false }))
					}
				}
			}
			RpcCommand::GetForkMessages => {
				// Upstream `getUserMessagesForForking`: user messages with
				// their entry ids (`contentText` extraction).
				let guard = self.log.lock().unwrap();
				let messages: Vec<Value> = guard
					.session
					.entries()
					.iter()
					.filter_map(|entry| {
						let id = entry.id()?;
						match &entry.kind {
							EntryKind::Message { message } => match message.as_ref() {
								pi_ai::Message::User(user) => {
									let text = user.content.as_text();
									(!text.is_empty())
										.then(|| json!({ "entryId": id, "text": text }))
								}
								_ => None,
							},
							_ => None,
						}
					})
					.collect();
				success_frame(&id, "get_fork_messages", json!({ "messages": messages }))
			}
			RpcCommand::GetEntries { since } => {
				let guard = self.log.lock().unwrap();
				let entries = guard.session.entries();
				let suffix = match &since {
					None => entries,
					Some(since) => {
						let index = entries
							.iter()
							.position(|entry| entry.id() == Some(since.as_str()));
						match index {
							None => {
								return error_frame(
									&id,
									"get_entries",
									format!("Entry not found: {since}"),
								);
							}
							Some(index) => &entries[index + 1..],
						}
					}
				};
				let values: Vec<Value> = suffix.iter().map(SessionEntry::to_value).collect();
				let leaf = guard.session.leaf_id().map(str::to_owned);
				success_frame(
					&id,
					"get_entries",
					json!({ "entries": values, "leafId": leaf }),
				)
			}
			RpcCommand::GetTree => {
				let guard = self.log.lock().unwrap();
				let tree: Vec<Value> = guard
					.session
					.get_tree()
					.iter()
					.map(SessionTreeNode::to_value)
					.collect();
				let leaf = guard.session.leaf_id().map(str::to_owned);
				success_frame(&id, "get_tree", json!({ "tree": tree, "leafId": leaf }))
			}
			RpcCommand::GetSessionStats => {
				success_frame(&id, "get_session_stats", self.session_stats_value())
			}
			RpcCommand::ExportHtml { output_path } => {
				// Upstream goes through `session.exportToHtml`, which reads the
				// persisted log from disk; the shared log's session dir is the
				// live equivalent here (and follows session replacement).
				let session_dir = self
					.log
					.lock()
					.unwrap()
					.session
					.session_dir()
					.map(Path::to_path_buf);
				match session_dir {
					None => error_frame(
						&id,
						"export_html",
						"Cannot export in-memory session to HTML",
					),
					Some(dir) => {
						match crate::core::export_html::write_session_html(
							&dir,
							output_path.as_deref().map(Path::new),
						) {
							Ok(path) => success_frame(
								&id,
								"export_html",
								json!({ "path": path.to_string_lossy() }),
							),
							Err(error) => error_frame(&id, "export_html", error),
						}
					}
				}
			}
			RpcCommand::SetSessionName { name } => {
				let name = name.trim();
				if name.is_empty() {
					error_frame(&id, "set_session_name", "Session name cannot be empty")
				} else {
					let _ = self.cmd_tx.send(SessionCommand::SetSessionName {
						name: name.to_string(),
					});
					success_frame(&id, "set_session_name", Value::Null)
				}
			}
			RpcCommand::SetAutoRetry { enabled } => {
				let _ = self.cmd_tx.send(SessionCommand::SetAutoRetry { enabled });
				success_frame(&id, "set_auto_retry", Value::Null)
			}
			RpcCommand::AbortRetry => {
				let _ = self.cmd_tx.send(SessionCommand::AbortRetry);
				success_frame(&id, "abort_retry", Value::Null)
			}
			RpcCommand::AbortBash => {
				let _ = self.cmd_tx.send(SessionCommand::AbortBash);
				success_frame(&id, "abort_bash", Value::Null)
			}
			// `bash` is intercepted in `handle_line` (its response frame is
			// emitted when the execution finishes); this arm only serves a
			// direct `dispatch` call.
			RpcCommand::Bash { .. } => {
				error_frame(&id, "bash", "bash responses are emitted asynchronously")
			}
		}
	}

	fn is_run_active(&self) -> bool {
		self.agent().is_streaming()
	}

	fn state_value(&self) -> Value {
		let agent = self.agent();
		let snapshot = agent.snapshot();
		let (steering_mode, follow_up_mode) = agent.queue_modes();
		let session_name = self.shared.session_name.lock().unwrap().clone();
		json!({
			"model": snapshot.model,
			"thinkingLevel": snapshot.thinking_level,
			"isStreaming": agent.is_streaming(),
			"isCompacting": self
				.compacting
				.load(std::sync::atomic::Ordering::Relaxed),
			"steeringMode": queue_mode_wire(steering_mode),
			"followUpMode": queue_mode_wire(follow_up_mode),
			"sessionFile": self
				.shared
				.current_session_file
				.lock()
				.unwrap()
				.as_ref()
				.map(|path| path.display().to_string()),
			"sessionId": agent.session_id(),
			"sessionName": session_name,
			"messageCount": snapshot.messages.len(),
			"pendingMessageCount": snapshot.queued_steering + snapshot.queued_follow_up,
			"autoCompactionEnabled": self
				.auto_compaction
				.load(std::sync::atomic::Ordering::Relaxed),
		})
	}

	/// `getSessionStats` upstream (`agent-session.ts:4243`): aggregates over
	/// ALL entries, including history compacted away, so token totals reflect
	/// what was billed. Upstream also reports `cost`/`contextUsage`; pi-rs
	/// does not track them yet (declared omission).
	fn session_stats_value(&self) -> Value {
		let guard = self.log.lock().unwrap();
		let session = &guard.session;
		let mut user_messages = 0u64;
		let mut assistant_messages = 0u64;
		let mut tool_calls = 0u64;
		let mut tool_results = 0u64;
		let mut total_messages = 0u64;
		let mut input = 0u64;
		let mut output = 0u64;
		let mut cache_read = 0u64;
		let mut cache_write = 0u64;
		let mut add_usage = |usage: &pi_ai::Usage| {
			input += usage.input;
			output += usage.output;
			cache_read += usage.cache_read;
			cache_write += usage.cache_write;
		};
		for entry in session.entries() {
			match &entry.kind {
				EntryKind::Usage { usage, .. } => add_usage(usage),
				// Model-attributed usage carried by summaries counts too.
				EntryKind::BranchSummary {
					usage: Some(usage), ..
				}
				| EntryKind::Compaction {
					usage: Some(usage), ..
				} => add_usage(usage),
				EntryKind::Message { message } => {
					total_messages += 1;
					match message.as_ref() {
						pi_ai::Message::User(_) => user_messages += 1,
						pi_ai::Message::ToolResult(result) => {
							tool_results += 1;
							if let Some(usage) = &result.usage {
								add_usage(usage);
							}
						}
						pi_ai::Message::Assistant(assistant) => {
							assistant_messages += 1;
							tool_calls += assistant
								.content
								.iter()
								.filter(|block| {
									matches!(block, pi_ai::AssistantContent::ToolCall(_))
								})
								.count() as u64;
							add_usage(&assistant.usage);
						}
						pi_ai::Message::System(_) => {}
					}
				}
				_ => {}
			}
		}
		let session_id = session
			.session_id()
			.or_else(|| session.header_id().map(str::to_owned))
			.unwrap_or_default();
		json!({
			"sessionFile": self
				.shared
				.current_session_file
				.lock()
				.unwrap()
				.as_ref()
				.map(|path| path.display().to_string()),
			"sessionId": session_id,
			"userMessages": user_messages,
			"assistantMessages": assistant_messages,
			"toolCalls": tool_calls,
			"toolResults": tool_results,
			"totalMessages": total_messages,
			"tokens": {
				"input": input,
				"output": output,
				"cacheRead": cache_read,
				"cacheWrite": cache_write,
				"total": input + output + cache_read + cache_write,
			},
		})
	}
}

/// Rename only event envelope fields; tool payloads and arbitrary user JSON
/// are opaque and must not be recursively rewritten.
fn agent_event_frame(event: &pi_agent::AgentEvent) -> String {
	if let pi_agent::AgentEvent::MessageUpdate {
		message,
		assistant_message_event,
	} = event
	{
		let pi_ai::Message::Assistant(message) = message else {
			unreachable!("message updates are always assistant messages");
		};
		let mut delta = assistant_message_event.clone();
		if delta["type"] == "toolcall_start" {
			let index = delta["contentIndex"].as_u64().unwrap_or(u64::MAX) as usize;
			if let Some(pi_ai::AssistantContent::ToolCall(call)) = message.content.get(index) {
				delta["id"] = json!(call.id);
				delta["toolName"] = json!(call.name);
			}
		}
		delta
			.as_object_mut()
			.expect("assistant event is an object")
			.remove("partial");
		return json!({"type": "message_update", "usage": message.usage, "assistantMessageEvent": delta}).to_string();
	}
	let mut value = serde_json::to_value(event).expect("agent events serialize");
	let object = value.as_object_mut().expect("agent event is an object");
	for (source, target) in [
		("tool_results", "toolResults"),
		("tool_call_id", "toolCallId"),
		("tool_name", "toolName"),
		("partial_result", "partialResult"),
		("is_error", "isError"),
		("duration_ms", "durationMs"),
	] {
		if let Some(field) = object.remove(source) {
			object.insert(target.into(), field);
		}
	}
	value.to_string()
}

/// The seven extended thinking levels in cycle order (upstream
/// `EXTENDED_THINKING_LEVELS`).
const ALL_THINKING_LEVELS: [ThinkingLevel; 7] = [
	ThinkingLevel::Off,
	ThinkingLevel::Minimal,
	ThinkingLevel::Low,
	ThinkingLevel::Medium,
	ThinkingLevel::High,
	ThinkingLevel::Xhigh,
	ThinkingLevel::Max,
];

/// Levels the current model supports — port of upstream
/// `getSupportedThinkingLevels` (`packages/ai/src/models.ts`): models without
/// reasoning support only get `off`. The upstream also filters `xhigh`/`max`
/// through the model's `thinkingLevelMap`; the Rust `Model` has no level map,
/// so reasoning models report every level.
fn supported_thinking_levels(model: &Model) -> Vec<ThinkingLevel> {
	if model.reasoning {
		ALL_THINKING_LEVELS.to_vec()
	} else {
		vec![ThinkingLevel::Off]
	}
}

/// Wire string for a thinking level (upstream kebab/camel style: `off`,
/// `xhigh`, … — matches `ThinkingLevel`'s camelCase serde renaming).
fn thinking_level_wire(level: ThinkingLevel) -> &'static str {
	match level {
		ThinkingLevel::Off => "off",
		ThinkingLevel::Minimal => "minimal",
		ThinkingLevel::Low => "low",
		ThinkingLevel::Medium => "medium",
		ThinkingLevel::High => "high",
		ThinkingLevel::Xhigh => "xhigh",
		ThinkingLevel::Max => "max",
	}
}

/// Wire string for a queue mode (`rpc-types.ts`: `"all" | "one-at-a-time"`).
fn queue_mode_wire(mode: pi_agent::QueueMode) -> &'static str {
	match mode {
		pi_agent::QueueMode::All => "all",
		pi_agent::QueueMode::OneAtATime => "one-at-a-time",
	}
}

/// Texts of queued user messages, for `clear_queue`'s response.
fn queued_texts(messages: Vec<pi_ai::Message>) -> Vec<String> {
	messages
		.iter()
		.map(|message| match message {
			pi_ai::Message::User(user) => user.content.as_text(),
			_ => String::new(),
		})
		.collect()
}

/// Cycle order for `cycle_thinking_level` — mirrors the private
/// `next_thinking_level` in `core/agent_session.rs` (upstream
/// `cycleThinkingLevel` walks `THINKING_LEVEL_OPTIONS` and wraps).
fn next_thinking_level(current: ThinkingLevel) -> ThinkingLevel {
	let index = ALL_THINKING_LEVELS
		.iter()
		.position(|level| *level == current)
		.unwrap_or(0);
	ALL_THINKING_LEVELS[(index + 1) % ALL_THINKING_LEVELS.len()]
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
		mock_handle_with_reasoning(false)
	}

	/// Harness with a mock model; `reasoning` toggles the model's reasoning
	/// capability, which thinking-level commands trim against.
	fn mock_handle_with_reasoning(reasoning: bool) -> (SessionHandle, Arc<Mutex<ModelRegistry>>) {
		let stream: pi_ai::StreamFn = Arc::new(
			|model: &pi_ai::Model,
			 context: &pi_ai::TranscriptContext,
			 options: pi_ai::StreamOptions| {
				pi_ai::providers::mock::stream(model, context, options)
			},
		);
		let mut model = Model::new("mock-echo", "Mock", "mock", "mock", "");
		model.reasoning = reasoning;
		let agent = Agent::new(AgentOptions {
			model,
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

	fn echo_assistant_for_stats() -> pi_ai::AssistantMessage {
		pi_ai::AssistantMessage {
			content: vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
				text: "answer".into(),
				text_signature: None,
			})],
			api: "mock".into(),
			provider: "mock".into(),
			model: "mock".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: pi_ai::Usage::default(),
			stop_reason: pi_ai::StopReason::Stop,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		}
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

	/// Collect event frame types until `agent_settled`, returning them in
	/// order (the trailing entry is always `agent_settled`).
	async fn until_settled(session: &mut RpcSession) -> Vec<String> {
		tokio::time::timeout(Duration::from_secs(5), async {
			let mut kinds = Vec::new();
			loop {
				let frame = session
					.next_event_frame()
					.await
					.expect("session stays open until settle");
				let kind = parse_frame(&frame)["type"]
					.as_str()
					.unwrap_or_default()
					.to_string();
				let settled = kind == "agent_settled";
				kinds.push(kind);
				if settled {
					return kinds;
				}
			}
		})
		.await
		.expect("settle should arrive in time")
	}

	/// Wait until the run is streaming (first `message_start`).
	async fn until_message_start(session: &mut RpcSession) {
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
		let frames = session.handle_line(r#"{"type":"teleport","id":"7"}"#);
		let frame = parse_frame(&frames[0]);
		assert_eq!(frame["success"], false);
		assert_eq!(frame["id"], "7");
		assert!(frame["error"].as_str().unwrap().contains("teleport"));
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
		assert_eq!(data["isCompacting"], false);
		// Both queues default to "one-at-a-time" (upstream agent.ts).
		assert_eq!(data["steeringMode"], "one-at-a-time");
		assert_eq!(data["followUpMode"], "one-at-a-time");
		assert_eq!(data["thinkingLevel"], "off");
		assert_eq!(data["messageCount"], 1); // leading system message
		assert_eq!(data["pendingMessageCount"], 0);
		assert_eq!(data["autoCompactionEnabled"], true);
		assert!(data["model"]["provider"].is_string());
	}

	#[tokio::test]
	async fn queue_modes_roundtrip_through_get_state() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session.handle_line(r#"{"type":"set_steering_mode","mode":"all"}"#);
		assert_eq!(parse_frame(&frames[0])["success"], true);
		let frames = session.handle_line(r#"{"type":"set_follow_up_mode","mode":"all"}"#);
		assert_eq!(parse_frame(&frames[0])["success"], true);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(frame["data"]["steeringMode"], "all");
		assert_eq!(frame["data"]["followUpMode"], "all");
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
		assert_eq!(session.agent().thinking_level(), ThinkingLevel::High);

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

		// A fuzzy match must not accept a different provider or model ID.
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_model","provider":"mock","modelId":"mock"}"#)[0],
		);
		assert_eq!(frame["success"], false);
	}

	#[test]
	fn agent_event_wire_fields_are_camel_case_without_rewriting_payloads() {
		use pi_agent::AgentEvent;
		let frame = parse_frame(&agent_event_frame(&AgentEvent::ToolExecutionUpdate {
			tool_call_id: "call-1".into(),
			tool_name: "test".into(),
			args: json!({"user_key": 1}),
			partial_result: json!({"nested_key": 2}),
		}));
		assert_eq!(
			frame,
			json!({"type":"tool_execution_update", "toolCallId":"call-1",
			"toolName":"test", "args":{"user_key":1}, "partialResult":{"nested_key":2}})
		);
		let frame = parse_frame(&agent_event_frame(&AgentEvent::ToolExecutionEnd {
			tool_call_id: "call-1".into(),
			tool_name: "test".into(),
			result: json!({}),
			is_error: false,
			duration_ms: Some(5),
		}));
		assert_eq!(frame["isError"], false);
		assert_eq!(frame["durationMs"], 5);
		assert!(frame.get("is_error").is_none());
	}

	#[tokio::test]
	async fn get_available_models_lists_the_registry() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_available_models"}"#)[0]);
		assert_eq!(frame["success"], true);
		assert!(!frame["data"]["models"].as_array().unwrap().is_empty());
	}

	#[tokio::test]
	async fn available_thinking_levels_follow_the_model_reasoning_capability() {
		// A model without reasoning support only ever gets `off`.
		let (handle, registry) = mock_handle_with_reasoning(false);
		let session = RpcSession::new(handle, registry);
		let frame =
			parse_frame(&session.handle_line(r#"{"type":"get_available_thinking_levels"}"#)[0]);
		assert_eq!(frame["data"]["levels"], json!(["off"]));

		// Reasoning models report the full extended ladder (the Rust `Model`
		// has no `thinkingLevelMap`, so nothing further is filtered).
		let (handle, registry) = mock_handle_with_reasoning(true);
		let session = RpcSession::new(handle, registry);
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
	async fn queue_modes_validate_wire_values_and_apply_to_agent() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_steering_mode","mode":"all","id":"s"}"#)[0],
		);
		assert_eq!(
			frame,
			json!({"id":"s","type":"response","command":"set_steering_mode","success":true})
		);
		session.agent().steer("one");
		session.agent().steer("two");
		assert_eq!(session.agent().peek_queued_messages().len(), 2);
		session.agent().clear_all_queues();
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_follow_up_mode","mode":"one-at-a-time"}"#)[0],
		);
		assert_eq!(frame["success"], true);
		session.agent().follow_up("one");
		session.agent().follow_up("two");
		assert_eq!(session.agent().peek_queued_messages().len(), 1);
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_follow_up_mode","mode":"invalid"}"#)[0],
		);
		assert_eq!(frame["success"], false);
	}

	#[tokio::test]
	async fn last_assistant_text_matches_upstream_empty_and_completed_shapes() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_last_assistant_text"}"#)[0]);
		// Upstream wire: `data: { text: string | null }` — empty stays a success.
		assert_eq!(frame["data"], json!({"text": null}));
		session.handle_line(r#"{"type":"prompt","message":"hello"}"#);
		until_agent_end(&mut session).await;
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_last_assistant_text"}"#)[0]);
		assert!(frame["data"]["text"].as_str().unwrap().contains("hello"));
	}

	#[tokio::test]
	async fn clear_queue_drains_and_reports_the_queued_texts() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		// `all` mode makes the per-queue peek report every queued text; the
		// default `one-at-a-time` peek below shows the mode limit.
		session.agent().set_steering_mode(pi_agent::QueueMode::All);
		session.agent().set_follow_up_mode(pi_agent::QueueMode::All);
		session.agent().steer("one");
		session.agent().steer("two");
		session.agent().follow_up("later-1");
		session.agent().follow_up("later-2");
		let frame = parse_frame(&session.handle_line(r#"{"type":"clear_queue","id":"c1"}"#)[0]);
		assert_eq!(frame["id"], "c1");
		assert_eq!(frame["success"], true);
		// The drained texts are attributed per queue, like upstream
		// `session.clearQueue()`.
		assert_eq!(frame["data"]["steering"], json!(["one", "two"]));
		assert_eq!(frame["data"]["followUp"], json!(["later-1", "later-2"]));
		assert!(session.agent().peek_queued_messages().is_empty());
		assert!(session.agent().peek_steering_queue().is_empty());
		assert!(session.agent().peek_follow_up_queue().is_empty());
	}

	#[tokio::test]
	async fn clear_queue_attributes_the_next_message_per_queue_in_one_at_a_time_mode() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		session.agent().steer("first steering");
		session.agent().steer("second steering");
		session.agent().follow_up("first follow-up");
		let frame = parse_frame(&session.handle_line(r#"{"type":"clear_queue"}"#)[0]);
		// The per-queue peek reports what the current queue mode would deliver
		// next (upstream reports the whole queue; see the module doc note).
		assert_eq!(frame["data"]["steering"], json!(["first steering"]));
		assert_eq!(frame["data"]["followUp"], json!(["first follow-up"]));
		// Everything queued is cleared either way.
		assert!(session.agent().peek_queued_messages().is_empty());
		assert_eq!(session.agent().snapshot().queued_steering, 0);
		assert_eq!(session.agent().snapshot().queued_follow_up, 0);
	}

	#[tokio::test]
	async fn cycle_model_moves_off_the_current_registry_model() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		assert_eq!(session.agent().model().id, "mock-echo");
		let frame = parse_frame(&session.handle_line(r#"{"type":"cycle_model","id":"m"}"#)[0]);
		assert_eq!(frame["command"], "cycle_model");
		assert_eq!(frame["success"], true);
		assert_eq!(frame["id"], "m");
		assert!(frame["data"]["provider"].is_string());
		assert!(frame["data"]["id"].is_string());
		assert_ne!(frame["data"]["id"], "mock-echo");
		// Applied by the session task, which announces the switch.
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = session.next_event_frame().await.unwrap();
				if frame.contains("model →") {
					break;
				}
			}
		})
		.await
		.expect("model notice should arrive in time");
		assert_ne!(session.agent().model().id, "mock-echo");
	}

	#[tokio::test]
	async fn cycle_thinking_level_walks_the_ladder_and_wraps() {
		let (handle, registry) = mock_handle_with_reasoning(true);
		let mut session = RpcSession::new(handle, registry);
		assert_eq!(session.agent().thinking_level(), ThinkingLevel::Off);
		// Each cycle reads the level the session task last applied, so the
		// next command must wait for the previous notice (same visibility a
		// real RPC client has).
		for expected in ["minimal", "low", "medium", "high", "xhigh", "max", "off"] {
			let frame = parse_frame(&session.handle_line(r#"{"type":"cycle_thinking_level"}"#)[0]);
			assert_eq!(frame["data"]["level"], expected);
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
		}
		assert_eq!(session.agent().thinking_level(), ThinkingLevel::Off);
	}

	#[tokio::test]
	async fn cycle_thinking_level_is_a_noop_for_models_without_reasoning() {
		let (handle, registry) = mock_handle_with_reasoning(false);
		let session = RpcSession::new(handle, registry);
		// Upstream `cycleThinkingLevel` returns undefined → success with a
		// null data field, and the level stays where it was.
		let frame = parse_frame(&session.handle_line(r#"{"type":"cycle_thinking_level"}"#)[0]);
		assert_eq!(frame["command"], "cycle_thinking_level");
		assert_eq!(frame["success"], true);
		assert!(frame.get("data").is_none() || frame["data"].is_null());
		assert_eq!(session.agent().thinking_level(), ThinkingLevel::Off);
		// No thinking notice is emitted (nothing changed); the drain helper
		// would hang, so only the agent state is asserted.
		session.shutdown();
	}

	#[tokio::test]
	async fn compact_reports_prompt_style_disposition() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"compact","id":"k"}"#)[0]);
		assert_eq!(frame["command"], "compact");
		assert_eq!(frame["success"], true);
		assert_eq!(frame["id"], "k");
		assert_eq!(frame["data"]["disposition"], "started");
	}

	#[tokio::test]
	async fn set_auto_compaction_toggles_the_state_flag() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_auto_compaction","enabled":false,"id":"a"}"#)[0],
		);
		assert_eq!(frame["command"], "set_auto_compaction");
		assert_eq!(frame["success"], true);
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = session.next_event_frame().await.unwrap();
				if frame.contains("auto-compaction disabled") {
					break;
				}
			}
		})
		.await
		.expect("notice should arrive in time");
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(frame["data"]["autoCompactionEnabled"], false);

		session.handle_line(r#"{"type":"set_auto_compaction","enabled":true}"#);
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = session.next_event_frame().await.unwrap();
				if frame.contains("auto-compaction enabled") {
					break;
				}
			}
		})
		.await
		.expect("notice should arrive in time");
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(frame["data"]["autoCompactionEnabled"], true);
	}

	#[tokio::test]
	async fn get_commands_lists_builtin_slash_commands_as_prompt_source() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_commands"}"#)[0]);
		assert_eq!(frame["success"], true);
		let commands = frame["data"]["commands"].as_array().unwrap();
		assert!(!commands.is_empty());
		assert!(commands.iter().any(|command| {
			command["name"] == "help"
				&& command["source"] == "prompt"
				&& command["description"].is_string()
		}));
	}

	// -- session-tree commands ------------------------------------------------

	/// Harness with a persisted session directory.
	fn mock_handle_with_session(tag: &str) -> (SessionHandle, Arc<Mutex<ModelRegistry>>) {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-rpc-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		let stream: pi_ai::StreamFn = Arc::new(
			|model: &pi_ai::Model,
			 context: &pi_ai::TranscriptContext,
			 options: pi_ai::StreamOptions| {
				pi_ai::providers::mock::stream(model, context, options)
			},
		);
		let model = Model::new("mock-echo", "Mock", "mock", "mock", "");
		let agent = Agent::new(AgentOptions {
			model,
			stream_fn: Some(stream),
			..AgentOptions::default()
		});
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::session_manager::SessionManager::open(&dir).unwrap(),
			registry.clone(),
		);
		(handle, registry)
	}

	fn temp_rpc_dir(tag: &str) -> std::path::PathBuf {
		std::env::temp_dir().join(format!(
			"pi-rs-rpc-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		))
	}

	#[tokio::test]
	async fn get_entries_lists_entries_and_supports_since() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_entries","id":"e"}"#)[0]);
		assert_eq!(frame["success"], true);
		let entries = frame["data"]["entries"].as_array().unwrap();
		assert!(
			entries.iter().all(|entry| entry.get("id").is_some()),
			"wire entries carry the tree base fields"
		);
		assert_eq!(frame["data"]["leafId"], entries.last().unwrap()["id"]);
		// `since` returns the suffix after the named entry.
		let first = entries[0]["id"].as_str().unwrap();
		let frame = parse_frame(
			&session.handle_line(&format!(r#"{{"type":"get_entries","since":"{first}"}}"#))[0],
		);
		let suffix = frame["data"]["entries"].as_array().unwrap();
		assert_eq!(suffix.len(), entries.len() - 1);
		assert!(suffix.iter().all(|entry| entry["id"] != first));
		// Unknown ids are an error frame.
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"get_entries","since":"nosuch","id":"z"}"#)[0],
		);
		assert_eq!(frame["success"], false);
		assert_eq!(frame["id"], "z");
		assert!(frame["error"].as_str().unwrap().contains("nosuch"));
		session.shutdown();
	}

	#[tokio::test]
	async fn get_tree_returns_the_forest_and_leaf() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_tree","id":"t"}"#)[0]);
		assert_eq!(frame["success"], true);
		let tree = frame["data"]["tree"].as_array().unwrap();
		assert!(!tree.is_empty());
		assert!(tree[0]["entry"]["id"].is_string());
		assert!(tree[0]["children"].is_array());
		// The leaf is the newest entry (the startup thinking-level entry).
		let entries =
			parse_frame(&session.handle_line(r#"{"type":"get_entries"}"#)[0])["data"]["entries"]
				.as_array()
				.unwrap()
				.clone();
		assert_eq!(
			frame["data"]["leafId"],
			entries.last().unwrap()["id"],
			"leafId names the newest entry"
		);
		session.shutdown();
	}

	#[tokio::test]
	async fn get_fork_messages_lists_user_texts_with_entry_ids() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"fork me"}"#);
		until_agent_end(&mut session).await;
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_fork_messages"}"#)[0]);
		assert_eq!(frame["success"], true);
		let messages = frame["data"]["messages"].as_array().unwrap();
		assert_eq!(messages.len(), 1);
		assert_eq!(messages[0]["text"], "fork me");
		assert!(messages[0]["entryId"].is_string());
		session.shutdown();
	}

	#[tokio::test]
	async fn set_session_name_rejects_empty_and_emits_session_info_changed() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_session_name","name":"  ","id":"n"}"#)[0],
		);
		assert_eq!(frame["success"], false);
		assert_eq!(frame["error"], "Session name cannot be empty");

		let frame = parse_frame(
			&session
				.handle_line(r#"{"type":"set_session_name","name":"Refactor\nauth","id":"n2"}"#)[0],
		);
		assert_eq!(frame["success"], true);
		// The change rides the wire as session_info_changed with the
		// sanitized name.
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "session_info_changed" {
					assert_eq!(frame["name"], "Refactor auth");
					break;
				}
			}
		})
		.await
		.expect("session_info_changed should arrive");
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(frame["data"]["sessionName"], "Refactor auth");
		session.shutdown();
	}

	#[tokio::test]
	async fn get_session_stats_aggregates_all_entries() {
		let (handle, registry) = mock_handle();
		{
			let mut guard = handle.log.lock().unwrap();
			guard
				.session
				.append_message(&pi_ai::Message::user("question"))
				.unwrap();
			let mut assistant = echo_assistant_for_stats();
			assistant.usage.input = 10;
			assistant.usage.output = 5;
			guard
				.session
				.append_message(&pi_ai::Message::Assistant(assistant))
				.unwrap();
			let result = pi_ai::ToolResultMessage {
				tool_call_id: "call-1".into(),
				tool_name: "bash".into(),
				content: vec![pi_ai::TextOrImage::text("out")],
				usage: Some(pi_ai::Usage {
					input: 2,
					..Default::default()
				}),
				is_error: false,
				timestamp: 1,
				duration_ms: None,
				details: None,
			};
			guard
				.session
				.append_message(&pi_ai::Message::ToolResult(result))
				.unwrap();
			// A usage entry and a compacted-away assistant both count.
			guard
				.session
				.append(SessionEntry::new(
					crate::core::session_manager::EntryKind::Usage {
						kind: "cache_warm".into(),
						provider: "mock".into(),
						model: "mock".into(),
						usage: pi_ai::Usage {
							cache_read: 7,
							..Default::default()
						},
						note: None,
					},
				))
				.unwrap();
		}
		let session = RpcSession::new(handle, registry);
		let frame =
			parse_frame(&session.handle_line(r#"{"type":"get_session_stats","id":"s"}"#)[0]);
		assert_eq!(frame["success"], true);
		let data = &frame["data"];
		assert_eq!(data["userMessages"], 1);
		assert_eq!(data["assistantMessages"], 1);
		assert_eq!(data["toolResults"], 1);
		assert_eq!(data["totalMessages"], 3);
		assert_eq!(data["toolCalls"], 0);
		assert_eq!(data["tokens"]["input"], 12);
		assert_eq!(data["tokens"]["output"], 5);
		assert_eq!(data["tokens"]["cacheRead"], 7);
		assert_eq!(data["tokens"]["total"], 24);
		assert!(data["sessionFile"].is_null() || data["sessionFile"].is_string());
	}

	#[tokio::test]
	async fn fork_frames_validate_entry_and_unsaved_session() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		// In-memory session: fork/clone refuse until the session is saved.
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"fork","entryId":"whatever","id":"f"}"#)[0],
		);
		assert_eq!(frame["success"], false);
		assert!(
			frame["error"]
				.as_str()
				.unwrap()
				.contains("has not been saved yet")
		);
		// The in-memory session has startup entries, so the leaf exists — the
		// unsaved refusal is what the frame must carry.
		let frame = parse_frame(&session.handle_line(r#"{"type":"clone","id":"c"}"#)[0]);
		assert_eq!(frame["success"], false);
		assert!(
			frame["error"]
				.as_str()
				.unwrap()
				.contains("has not been saved yet")
		);
		session.shutdown();
	}

	#[tokio::test]
	async fn export_html_writes_the_document_and_reports_the_path() {
		let (handle, registry) = mock_handle_with_session("export-html");
		let session_dir = {
			let mut guard = handle.log.lock().unwrap();
			guard
				.session
				.append_message(&pi_ai::Message::user("export me"))
				.unwrap();
			guard.session.session_dir().unwrap().to_path_buf()
		};
		// Explicit output paths only: the default derivation writes into the
		// process cwd (upstream `exportToHtml`), which would litter the crate
		// dir during tests — `export_html.rs` unit tests cover that path.
		let session = RpcSession::new(handle, registry);
		let explicit = session_dir.join("named-export.html");
		let frame = parse_frame(
			&session.handle_line(
				json!({"type":"export_html","id":"e2","outputPath":explicit.to_string_lossy()})
					.to_string()
					.as_str(),
			)[0],
		);
		assert_eq!(frame["success"], true, "frame: {frame}");
		assert_eq!(frame["data"]["path"], explicit.to_string_lossy().as_ref());
		let html = std::fs::read_to_string(&explicit).unwrap();
		assert!(html.contains("<!DOCTYPE html>"));
		assert!(html.contains("export me"));
		// Relative output paths resolve against the process cwd like upstream.
		std::env::set_current_dir(&session_dir).unwrap();
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"export_html","id":"e3","outputPath":"rel.html"}"#)[0],
		);
		assert_eq!(frame["success"], true, "frame: {frame}");
		assert!(session_dir.join("rel.html").exists());
		std::env::set_current_dir(std::env::temp_dir()).unwrap();
		session.shutdown();
		std::fs::remove_dir_all(&session_dir).ok();
	}
	#[tokio::test]
	async fn fork_and_clone_answer_with_cancelled_false_on_a_saved_session() {
		let (handle, registry) = mock_handle_with_session("fork-frames");
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"first question"}"#);
		until_agent_end(&mut session).await;
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_fork_messages"}"#)[0]);
		let entry_id = frame["data"]["messages"][0]["entryId"]
			.as_str()
			.unwrap()
			.to_string();

		// fork(Before) at the first user message answers with its text.
		let frame = parse_frame(
			&session.handle_line(&format!(
				r#"{{"type":"fork","entryId":"{entry_id}","id":"f1"}}"#
			))[0],
		);
		assert_eq!(frame["success"], true);
		assert_eq!(frame["id"], "f1");
		assert_eq!(frame["data"]["cancelled"], false);
		assert_eq!(frame["data"]["text"], "first question");
		let fork_notice = tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "notice"
					&& frame["message"]
						.as_str()
						.unwrap_or("")
						.starts_with("session → ")
				{
					break frame["message"].as_str().unwrap().to_string();
				}
			}
		})
		.await
		.expect("the fork replacement should finish");
		assert!(!fork_notice.is_empty());
		// get_state follows the branched session.
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		let branched = frame["data"]["sessionFile"].as_str().unwrap().to_string();
		assert!(branched.ends_with("session.jsonl"));

		// Prompt the branched session, then clone at the leaf.
		session.handle_line(r#"{"type":"prompt","message":"edited"}"#);
		until_agent_end(&mut session).await;
		let frame = parse_frame(&session.handle_line(r#"{"type":"clone","id":"c1"}"#)[0]);
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["cancelled"], false);
		// Wait for the clone's replacement to land before probing state (the
		// fork's notice was already consumed above).
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "notice"
					&& frame["message"]
						.as_str()
						.unwrap_or("")
						.starts_with("session → ")
				{
					break;
				}
			}
		})
		.await
		.expect("the clone replacement should finish");
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		let cloned = frame["data"]["sessionFile"].as_str().unwrap().to_string();
		assert_ne!(cloned, branched, "the clone is its own session file");
		session.shutdown_settled().await;
		// Best-effort cleanup of the created sessions.
		for path in [branched, cloned] {
			if let Some(dir) = std::path::Path::new(&path).parent() {
				let _ = std::fs::remove_dir_all(dir);
			}
		}
		let _ = std::fs::remove_dir_all(temp_rpc_dir("fork-frames"));
	}

	#[tokio::test]
	async fn switch_session_frame_validates_then_answers_cancelled_false() {
		let (handle, registry) = mock_handle_with_session("switch-frame");
		let mut session = RpcSession::new(handle, registry);
		// A missing target fails synchronously.
		let frame = parse_frame(
			&session.handle_line(
				r#"{"type":"switch_session","sessionPath":"/nonexistent/session","id":"w"}"#,
			)[0],
		);
		assert_eq!(frame["success"], false);
		// A valid target answers cancelled:false and the session swaps.
		let target = temp_rpc_dir("switch-target");
		std::fs::create_dir_all(&target).unwrap();
		{
			let mut target_session =
				crate::core::session_manager::SessionManager::open(&target).unwrap();
			target_session
				.append_message(&pi_ai::Message::user("switched history"))
				.unwrap();
		}
		let frame = parse_frame(
			&session.handle_line(&format!(
				r#"{{"type":"switch_session","sessionPath":"{}","id":"w2"}}"#,
				target.display()
			))[0],
		);
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["cancelled"], false);
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "notice"
					&& frame["message"]
						.as_str()
						.unwrap_or("")
						.starts_with("session → ")
				{
					break;
				}
			}
		})
		.await
		.expect("the switch should finish");
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(
			frame["data"]["sessionFile"],
			target.join("session.jsonl").display().to_string()
		);
		session.shutdown();
		let _ = std::fs::remove_dir_all(target);
		let _ = std::fs::remove_dir_all(temp_rpc_dir("switch-frame"));
	}

	#[tokio::test]
	async fn new_session_answers_cancelled_false_and_resets_the_state() {
		let (handle, registry) = mock_handle_with_session("new-frame");
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"old"}"#);
		until_agent_end(&mut session).await;
		let frame = parse_frame(&session.handle_line(r#"{"type":"new_session","id":"n"}"#)[0]);
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["cancelled"], false);
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "notice"
					&& frame["message"]
						.as_str()
						.unwrap_or("")
						.starts_with("session → ")
				{
					break;
				}
			}
		})
		.await
		.expect("the replacement should finish");
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(frame["data"]["messageCount"], 1, "fresh transcript");
		session.shutdown();
	}

	// -- bash passthrough frames ----------------------------------------------

	#[tokio::test]
	async fn bash_streams_updates_and_answers_with_the_result() {
		let (handle, registry) = mock_handle_with_session("bash-frame");
		let mut session = RpcSession::new(handle, registry);
		// The response frame is deferred until the execution completes.
		let frames =
			session.handle_line(r#"{"type":"bash","command":"printf frame-bash","id":"b9"}"#);
		assert!(frames.is_empty(), "bash answers asynchronously");
		let mut updates = 0;
		let response = loop {
			let frame = parse_frame(&session.next_event_frame().await.unwrap());
			if frame["type"] == "bash_execution_update" {
				updates += 1;
				assert_eq!(frame["id"], "b9");
				assert!(frame["delta"].is_string());
				continue;
			}
			break frame;
		};
		assert!(updates >= 1, "at least one update frame");
		assert_eq!(response["type"], "response");
		assert_eq!(response["command"], "bash");
		assert_eq!(response["success"], true);
		assert_eq!(response["id"], "b9");
		assert!(
			response["data"]["output"]
				.as_str()
				.unwrap()
				.contains("frame-bash")
		);
		assert_eq!(response["data"]["exitCode"], 0);
		assert_eq!(response["data"]["cancelled"], false);
		session.shutdown();
		let _ = std::fs::remove_dir_all(temp_rpc_dir("bash-frame"));
	}

	#[tokio::test]
	async fn abort_bash_answers_successfully() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frame = parse_frame(&session.handle_line(r#"{"type":"abort_bash","id":"ab"}"#)[0]);
		assert_eq!(frame["command"], "abort_bash");
		assert_eq!(frame["success"], true);
	}

	// -- retry frames ----------------------------------------------------------

	#[tokio::test]
	async fn set_auto_retry_and_abort_retry_answer_success() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		let frame = parse_frame(
			&session.handle_line(r#"{"type":"set_auto_retry","enabled":false,"id":"r"}"#)[0],
		);
		assert_eq!(frame["success"], true);
		tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "notice" && frame["message"] == "auto-retry disabled" {
					break;
				}
			}
		})
		.await
		.expect("the toggle notice should arrive");
		let frame = parse_frame(&session.handle_line(r#"{"type":"abort_retry","id":"r2"}"#)[0]);
		assert_eq!(frame["success"], true);
		session.shutdown();
	}

	#[tokio::test]
	async fn agent_end_frames_carry_will_retry() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"hello"}"#);
		let events = until_agent_end(&mut session).await;
		let end = events.last().unwrap();
		assert_eq!(end["type"], "agent_end");
		assert_eq!(end["willRetry"], false, "a successful run does not retry");
		let _ = until_settled(&mut session).await;
		session.shutdown();
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
		let update = events
			.iter()
			.find(|e| {
				e["type"] == "message_update" && e["assistantMessageEvent"]["type"] == "text_delta"
			})
			.expect("stream includes a text delta");
		assert!(update["assistantMessageEvent"]["delta"].is_string());
		assert!(update["usage"].is_object());
		assert!(update.get("message").is_none());
		assert!(update["assistantMessageEvent"].get("partial").is_none());
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
			.agent()
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
			.agent()
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
		assert!(!session.agent().is_streaming());
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

	#[tokio::test]
	async fn agent_settled_arrives_after_an_idle_run_completes() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"hello"}"#);

		let kinds = until_settled(&mut session).await;
		assert_eq!(kinds.last().map(String::as_str), Some("agent_settled"));
		assert_eq!(kinds.iter().filter(|k| *k == "agent_settled").count(), 1);
		assert!(kinds.contains(&"agent_end".to_string()));
		assert_eq!(
			kinds.iter().rev().nth(1).map(String::as_str),
			Some("agent_end"),
			"settle directly follows the run that went idle"
		);

		// The next run settles again — the event marks each busy→idle edge.
		session.handle_line(r#"{"type":"prompt","message":"again"}"#);
		let kinds = until_settled(&mut session).await;
		assert_eq!(kinds.last().map(String::as_str), Some("agent_settled"));
	}

	#[tokio::test]
	async fn agent_settled_waits_for_a_queued_follow_up_to_finish() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"first"}"#);
		// Queue the follow-up while the first turn is still streaming.
		until_message_start(&mut session).await;
		let frames = session.handle_line(r#"{"type":"follow_up","message":"second"}"#);
		assert_eq!(parse_frame(&frames[0])["data"]["disposition"], "queued");

		let kinds = until_settled(&mut session).await;
		// Exactly one settle, only after the follow-up turn completed: the
		// frames since queueing contain the follow-up's own assistant turn
		// and the run end — never a settle in between.
		assert_eq!(kinds.iter().filter(|k| *k == "agent_settled").count(), 1);
		assert!(kinds.contains(&"message_start".to_string()));
		assert_eq!(
			kinds.iter().rev().nth(1).map(String::as_str),
			Some("agent_end")
		);

		// The transcript confirms the follow-up turn ran to completion.
		let users: Vec<String> = session
			.agent()
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
	async fn agent_settled_waits_for_queued_prompts_too() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"first"}"#);
		until_message_start(&mut session).await;
		let frames = session.handle_line(r#"{"type":"prompt","message":"second"}"#);
		assert_eq!(parse_frame(&frames[0])["data"]["disposition"], "queued");

		// The first run ends, but the queued prompt starts immediately: the
		// second run's frames contain no settle.
		let first = until_agent_end(&mut session).await;
		assert!(
			first.iter().all(|f| f["type"] != "agent_settled"),
			"no settle while a queued prompt is pending: {:?}",
			first.iter().map(|f| f["type"].clone()).collect::<Vec<_>>()
		);
		let second = until_agent_end(&mut session).await;
		assert!(
			second.iter().all(|f| f["type"] != "agent_settled"),
			"no settle before the queued prompt's run ends: {:?}",
			second.iter().map(|f| f["type"].clone()).collect::<Vec<_>>()
		);
		// With both runs done and nothing queued, the session settles.
		let frame = parse_frame(
			&session
				.next_event_frame()
				.await
				.expect("settle follows the last run"),
		);
		assert_eq!(
			frame,
			json!({"type": "agent_settled", "aborted": false}),
			"the settle frame matches the upstream agent_settled shape"
		);
	}
}
