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
//!   responses wait for the session task to finish; failures are returned to
//!   the caller. Extensions can cancel upstream; this port has none, so a
//!   successful response always carries `cancelled: false`.
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

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle, WireEvent};
use crate::core::model_registry::ModelRegistry;
use crate::core::session_manager::{EntryKind, SessionEntry, SessionTreeNode};

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
	Compact {
		/// Upstream wire field (optional): custom focus for the summary.
		#[serde(default, rename = "customInstructions")]
		custom_instructions: Option<String>,
	},
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
	pending_replacements: std::sync::atomic::AtomicUsize,
	replacement_tx: mpsc::UnboundedSender<String>,
	replacement_rx: mpsc::UnboundedReceiver<String>,
	/// The extension bridge when one is attached: `extension_ui_response`
	/// frames resolve dialogs through it (round 44).
	extensions: Option<Arc<crate::core::extensions::ExtensionBridge>>,
	events_closed: bool,
	wire_events_closed: bool,
}

impl RpcSession {
	pub fn new(handle: SessionHandle, registry: Arc<Mutex<ModelRegistry>>) -> Self {
		let extensions = handle.resources().extensions.clone();
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
		let (replacement_tx, replacement_rx) = mpsc::unbounded_channel();
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
			pending_replacements: std::sync::atomic::AtomicUsize::new(0),
			replacement_tx,
			replacement_rx,
			extensions,
			events_closed: false,
			wire_events_closed: false,
		}
	}

	/// The agent to probe: the session task's live instance, so a session
	/// replacement is observable without re-binding.
	fn agent(&self) -> pi_agent::Agent {
		self.live_agent.lock().unwrap().clone()
	}

	/// Parse and dispatch one input line; returns the response frames to
	/// write (empty for blank lines and asynchronous bash/session replacement).
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
		// `extension_ui_response` is handled before command dispatch
		// (upstream rpc-mode.ts:771-780): resolve the pending dialog and
		// consume the frame silently — an unknown/expired id is ignored.
		if command_type == "extension_ui_response" {
			self.handle_extension_ui_response(&raw);
			return Vec::new();
		}
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
		let replacement = match &command {
			RpcCommand::NewSession { parent_session } => {
				Some(crate::core::agent_session::RpcReplacement::New {
					parent_session: parent_session.clone(),
				})
			}
			RpcCommand::SwitchSession { session_path } => {
				Some(crate::core::agent_session::RpcReplacement::Switch {
					path: session_path.clone(),
				})
			}
			RpcCommand::Fork { entry_id } => {
				Some(crate::core::agent_session::RpcReplacement::Fork {
					entry_id: entry_id.clone(),
				})
			}
			RpcCommand::Clone => Some(crate::core::agent_session::RpcReplacement::Clone),
			_ => None,
		};
		if let Some(operation) = replacement {
			let (response, received) = tokio::sync::oneshot::channel();
			self.pending_replacements
				.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			if self
				.cmd_tx
				.send(SessionCommand::RpcReplacement {
					operation,
					response,
				})
				.is_err()
			{
				self.pending_replacements
					.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
				return vec![error_frame(
					&id,
					&command_type,
					"session task is unavailable",
				)];
			}
			let output = self.replacement_tx.clone();
			tokio::spawn(async move {
				let frame = match received.await {
					Ok(Ok(text)) => {
						let mut data = json!({"cancelled": false});
						if let Some(text) = text {
							data["text"] = json!(text);
						}
						success_frame(&id, &command_type, data)
					}
					Ok(Err(error)) => error_frame(&id, &command_type, error),
					Err(_) => error_frame(
						&id,
						&command_type,
						"session task stopped before completing the request",
					),
				};
				let _ = output.send(frame);
			});
			return Vec::new();
		}
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
			if self.events_closed
				&& self
					.pending_replacements
					.load(std::sync::atomic::Ordering::Relaxed)
					== 0
			{
				return None;
			}
			tokio::select! {
				Some(frame) = self.replacement_rx.recv() => {
					self.pending_replacements.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
					return Some(frame);
				}
				event = self.events.recv(), if !self.events_closed => match event {
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
					Ok(SessionEvent::Persisted { .. } | SessionEvent::SessionReplaced) => continue,
					// Extension UI forwarding (round 44, upstream
					// rpc-mode.ts:60-248): dialogs surface as
					// `extension_ui_request` frames and the client answers
					// with `extension_ui_response` (handled in handle_line);
					// notify/setStatus/setTitle/set_editor_text are
					// fire-and-forget requests. The bridge only emits these
					// with dialog-capable UI, which RPC now declares.
					Ok(SessionEvent::ExtensionDialog(request)) => {
						return Some(extension_ui_request_frame(&request).to_string());
					}
					Ok(SessionEvent::ExtensionNotify { message, level }) => {
						return Some(extension_ui_notify_frame(&message, &level).to_string());
					}
					Ok(SessionEvent::ExtensionStatus { key, text }) => {
						let mut frame = json!({
							"type": "extension_ui_request",
							"id": uuid::Uuid::new_v4().to_string(),
							"method": "setStatus",
							"statusKey": key,
						});
						if let Some(text) = text {
							frame["statusText"] = json!(text);
						}
						return Some(frame.to_string());
					}
					Ok(SessionEvent::ExtensionTitle(title)) => {
						return Some(
							json!({
								"type": "extension_ui_request",
								"id": uuid::Uuid::new_v4().to_string(),
								"method": "setTitle",
								"title": title,
							})
							.to_string(),
						);
					}
					Ok(SessionEvent::ExtensionEditorText(text)) => {
						return Some(
							json!({
								"type": "extension_ui_request",
								"id": uuid::Uuid::new_v4().to_string(),
								"method": "set_editor_text",
								"text": text,
							})
							.to_string(),
						);
					}
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
					Err(broadcast::error::RecvError::Closed) => self.events_closed = true,
				},
				event = self.wire_events.recv(), if !self.wire_events_closed => match event {
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
					Err(broadcast::error::RecvError::Closed) => self.wire_events_closed = true,
				},
			}
		}
	}

	/// Resolve one `extension_ui_response` through the bridge's dialog
	/// registry (rpc-types.ts:294-298): `cancelled` answers the no-UI default
	/// (null), `confirmed` the boolean, `value` the string. Unknown or
	/// expired ids are ignored (`answer_dialog` reports false).
	fn handle_extension_ui_response(&self, raw: &Value) {
		let Some(bridge) = self.extensions.as_ref() else {
			return;
		};
		let Some(id) = raw
			.get("id")
			.and_then(Value::as_str)
			.and_then(|id| id.parse::<u64>().ok())
		else {
			return;
		};
		let answer = if raw.get("cancelled").and_then(Value::as_bool) == Some(true) {
			Value::Null
		} else if let Some(confirmed) = raw.get("confirmed").and_then(Value::as_bool) {
			Value::Bool(confirmed)
		} else if let Some(value) = raw.get("value").and_then(Value::as_str) {
			Value::String(value.to_string())
		} else {
			return;
		};
		bridge.answer_dialog(id, answer);
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
				let _ = self.cmd_tx.send(SessionCommand::Prompt {
					text: message,
					images: Vec::new(),
				});
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
			RpcCommand::Compact {
				custom_instructions,
			} => {
				// Same disposition inference as `prompt`; the session task
				// reports progress/refusals through notices.
				let disposition = if self.is_run_active() {
					"queued"
				} else {
					"started"
				};
				let _ = self.cmd_tx.send(SessionCommand::Compact {
					instructions: custom_instructions,
				});
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
			RpcCommand::NewSession { .. }
			| RpcCommand::SwitchSession { .. }
			| RpcCommand::Fork { .. }
			| RpcCommand::Clone => unreachable!("replacement commands are handled asynchronously"),
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
		let stats = crate::core::agent_session::compute_session_stats(
			&guard.session,
			self.shared.current_session_file.lock().unwrap().clone(),
		);
		json!({
			"sessionFile": stats
				.session_file
				.as_ref()
				.map(|path| path.display().to_string()),
			"sessionId": stats.session_id,
			"userMessages": stats.user_messages,
			"assistantMessages": stats.assistant_messages,
			"toolCalls": stats.tool_calls,
			"toolResults": stats.tool_results,
			"totalMessages": stats.total_messages,
			"tokens": {
				"input": stats.input,
				"output": stats.output,
				"cacheRead": stats.cache_read,
				"cacheWrite": stats.cache_write,
				"total": stats.input + stats.output + stats.cache_read + stats.cache_write,
			},
		})
	}
}

/// `extension_ui_request` frame for one dialog (rpc-types.ts:252-261). The
/// bridge's numeric dialog id travels as the string `id` — upstream uses
/// UUIDs, the client echoes whatever it receives. `confirm` carries a
/// `message` field on the wire; the host shim forwards only the title, so it
/// is empty (declared). Dialog `timeout` options are not passed by the host
/// shim and stay absent.
fn extension_ui_request_frame(request: &crate::core::extensions::ExtensionDialogRequest) -> Value {
	let id = request.id.to_string();
	match &request.kind {
		crate::core::extensions::ExtensionDialogKind::Select { title, options } => json!({
			"type": "extension_ui_request",
			"id": id,
			"method": "select",
			"title": title,
			"options": options,
		}),
		crate::core::extensions::ExtensionDialogKind::Confirm { title } => json!({
			"type": "extension_ui_request",
			"id": id,
			"method": "confirm",
			"title": title,
			"message": "",
		}),
		crate::core::extensions::ExtensionDialogKind::Input { title, placeholder } => {
			let mut frame = json!({
				"type": "extension_ui_request",
				"id": id,
				"method": "input",
				"title": title,
			});
			if let Some(placeholder) = placeholder {
				frame["placeholder"] = json!(placeholder);
			}
			frame
		}
	}
}

/// Fire-and-forget `extension_ui_request{method:"notify"}` frame
/// (rpc-mode.ts:152-162).
fn extension_ui_notify_frame(message: &str, level: &str) -> Value {
	json!({
		"type": "extension_ui_request",
		"id": uuid::Uuid::new_v4().to_string(),
		"method": "notify",
		"message": message,
		"notifyType": level,
	})
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
		let accepting_commands = session
			.pending_replacements
			.load(std::sync::atomic::Ordering::Relaxed)
			== 0;
		tokio::select! {
			line = line_rx.recv(), if accepting_commands => {
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

	async fn replacement_response(session: &mut RpcSession, request: &str) -> Value {
		let request_value: Value = serde_json::from_str(request).unwrap();
		let immediate = session.handle_line(request);
		if let Some(frame) = immediate.first() {
			return parse_frame(frame);
		}
		tokio::time::timeout(Duration::from_secs(10), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["type"] == "response"
					&& frame["command"] == request_value["type"]
					&& frame["id"] == request_value["id"]
				{
					return frame;
				}
			}
		})
		.await
		.unwrap()
	}

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

	/// RpcSession over a session with one node extension attached
	/// (`has_ui: true`, matching production RPC after round 44).
	async fn extension_session(
		fixture: &str,
	) -> (RpcSession, crate::core::extensions::ExtensionAssembly) {
		let entry = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("tests/fixtures/extensions")
			.join(fixture);
		let cwd = std::env::temp_dir().join(format!(
			"pi-rs-rpc-ext-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let _ = std::fs::remove_dir_all(&cwd);
		std::fs::create_dir_all(&cwd).unwrap();
		let assembly = crate::core::extensions::spawn_extensions(
			vec![(entry.display().to_string(), entry)],
			&crate::core::extensions::SpawnOptions {
				cwd,
				has_ui: true,
				runtime: "node".to_string(),
			},
		)
		.await;
		assert!(assembly.notices.is_empty(), "{:?}", assembly.notices);
		let stream: pi_ai::StreamFn = Arc::new(
			|model: &pi_ai::Model,
			 context: &pi_ai::TranscriptContext,
			 options: pi_ai::StreamOptions| {
				pi_ai::providers::mock::stream(model, context, options)
			},
		);
		let agent = Agent::new(AgentOptions {
			stream_fn: Some(stream),
			..AgentOptions::default()
		});
		let registry = Arc::new(Mutex::new(ModelRegistry::load(None)));
		let handle = crate::core::agent_session::spawn_agent_session_with_options(
			agent,
			crate::core::session_manager::SessionManager::in_memory(),
			registry.clone(),
			crate::core::settings_manager::CompactionSettings::default(),
			crate::core::system_prompt::SystemPromptOptions::default(),
			crate::core::settings_manager::RetrySettings::default(),
			crate::core::agent_session::SessionResources {
				extensions: Some(assembly.bridge.clone()),
				..Default::default()
			},
		);
		(RpcSession::new(handle, registry), assembly)
	}

	fn node_available() -> bool {
		std::process::Command::new("node")
			.arg("--version")
			.output()
			.is_ok()
	}

	/// A `ui.select` dialog from an extension command surfaces as an
	/// `extension_ui_request` frame and the client's `extension_ui_response`
	/// resolves it through the bridge registry (round 44).
	#[tokio::test(flavor = "multi_thread")]
	async fn extension_dialog_round_trips_through_the_rpc_wire() {
		if !node_available() {
			eprintln!("skipping: node not available");
			return;
		}
		let (mut session, assembly) = extension_session("dialogs.ts").await;
		session.handle_line(r#"{"type":"prompt","message":"/ask","id":"p1"}"#);

		let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
		let mut answered = false;
		let mut notify_seen = false;
		let mut settled_seen = false;
		while tokio::time::Instant::now() < deadline && !(notify_seen && settled_seen) {
			let frame = match tokio::time::timeout_at(deadline, session.next_event_frame()).await {
				Ok(Some(frame)) => frame,
				Ok(None) => break,
				Err(_) => break,
			};
			let value: Value = serde_json::from_str(&frame).unwrap();
			if value["type"] == "extension_ui_request" && value["method"] == "select" {
				assert_eq!(value["title"], "Pick a flavor");
				assert_eq!(value["options"], json!(["vanilla", "chocolate"]), "{value}");
				assert!(!answered, "the select dialog must not repeat: {value}");
				answered = true;
				let response = json!({
					"type": "extension_ui_response",
					"id": value["id"].clone(),
					"value": "chocolate",
				});
				assert!(
					session.handle_line(&response.to_string()).is_empty(),
					"the response is consumed without output frames"
				);
			} else if value["type"] == "extension_ui_request" && value["method"] == "notify" {
				assert_eq!(value["message"], "picked: chocolate", "{value}");
				assert_eq!(value["notifyType"], "info");
				notify_seen = true;
			} else if value["type"] == "agent_settled" {
				settled_seen = true;
			}
		}
		assert!(answered, "select dialog never surfaced");
		assert!(notify_seen, "notify frame never surfaced");
		assert!(settled_seen, "settle never surfaced");
		assembly.bridge.shutdown().await;
	}

	/// Fire-and-forget ui.* calls forward as `extension_ui_request` frames in
	/// RPC mode (upstream rpc-mode.ts:166-246); keyed statuses carry
	/// `statusKey`/`statusText`.
	#[tokio::test(flavor = "multi_thread")]
	async fn extension_decorations_forward_as_extension_ui_requests() {
		if !node_available() {
			eprintln!("skipping: node not available");
			return;
		}
		let (mut session, assembly) = extension_session("ui-decorations.ts").await;
		session.handle_line(r#"{"type":"prompt","message":"/decorate","id":"p1"}"#);

		let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
		let mut status = None;
		let mut title = None;
		let mut editor = None;
		let mut notify = None;
		while tokio::time::Instant::now() < deadline
			&& (status.is_none() || title.is_none() || editor.is_none() || notify.is_none())
		{
			let frame = match tokio::time::timeout_at(deadline, session.next_event_frame()).await {
				Ok(Some(frame)) => frame,
				Ok(None) => break,
				Err(_) => break,
			};
			let value: Value = serde_json::from_str(&frame).unwrap();
			if value["type"] != "extension_ui_request" {
				continue;
			}
			match value["method"].as_str() {
				Some("setStatus") => {
					status = Some((
						value["statusKey"].as_str().unwrap_or_default().to_string(),
						value["statusText"].as_str().map(str::to_string),
					));
				}
				Some("setTitle") => {
					title = Some(value["title"].as_str().unwrap_or_default().to_string())
				}
				Some("set_editor_text") => {
					editor = Some(value["text"].as_str().unwrap_or_default().to_string())
				}
				Some("notify") => {
					notify = Some((
						value["message"].as_str().unwrap_or_default().to_string(),
						value["notifyType"].as_str().unwrap_or_default().to_string(),
					));
				}
				_ => {}
			}
		}
		assert_eq!(
			status,
			Some(("files".to_string(), Some("watching files".to_string()))),
			"{status:?}"
		);
		assert_eq!(title.as_deref(), Some("pi-ext-title"));
		assert_eq!(editor.as_deref(), Some("prefilled text"));
		assert_eq!(notify, Some(("decorated".to_string(), "info".to_string())));
		assembly.bridge.shutdown().await;
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
		let mut session = RpcSession::new(handle, registry);
		// In-memory session: fork/clone refuse until the session is saved.
		let frame = replacement_response(
			&mut session,
			r#"{"type":"fork","entryId":"whatever","id":"f"}"#,
		)
		.await;
		assert_eq!(frame["success"], false);
		assert!(
			frame["error"]
				.as_str()
				.unwrap()
				.contains("Invalid entry ID")
		);
		// The in-memory session has startup entries, so the leaf exists — the
		// unsaved refusal is what the frame must carry.
		let frame = replacement_response(&mut session, r#"{"type":"clone","id":"c"}"#).await;
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
		let frame = replacement_response(
			&mut session,
			&format!(r#"{{"type":"fork","entryId":"{entry_id}","id":"f1"}}"#),
		)
		.await;
		assert_eq!(frame["success"], true);
		assert_eq!(frame["id"], "f1");
		assert_eq!(frame["data"]["cancelled"], false);
		assert_eq!(frame["data"]["text"], "first question");
		// get_state follows the branched session.
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		let branched = frame["data"]["sessionFile"].as_str().unwrap().to_string();
		assert!(branched.ends_with("session.jsonl"));

		// Prompt the branched session, then clone at the leaf.
		session.handle_line(r#"{"type":"prompt","message":"edited"}"#);
		until_agent_end(&mut session).await;
		let frame = replacement_response(&mut session, r#"{"type":"clone","id":"c1"}"#).await;
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["cancelled"], false);
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
		let frame = replacement_response(
			&mut session,
			r#"{"type":"switch_session","sessionPath":"/nonexistent/session","id":"w"}"#,
		)
		.await;
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
		let frame = replacement_response(
			&mut session,
			&format!(
				r#"{{"type":"switch_session","sessionPath":"{}","id":"w2"}}"#,
				target.display()
			),
		)
		.await;
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["cancelled"], false);
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
		let frame = replacement_response(&mut session, r#"{"type":"new_session","id":"n"}"#).await;
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["cancelled"], false);
		let frame = parse_frame(&session.handle_line(r#"{"type":"get_state"}"#)[0]);
		assert_eq!(frame["data"]["messageCount"], 1, "fresh transcript");
		session.shutdown();
	}

	#[tokio::test]
	async fn switch_reports_save_failure_and_keeps_original_state() {
		let root = temp_rpc_dir("switch-save-failure");
		std::fs::create_dir_all(&root).unwrap();
		let target =
			crate::core::session_manager::SessionManager::create_with_parent_in(&root, &root, None)
				.unwrap();
		let target_path = target.session_dir().unwrap().to_path_buf();
		let readonly_path = root.join("readonly.jsonl");
		std::fs::write(&readonly_path, "").unwrap();
		let (handle, registry) = mock_handle();
		{
			let mut log = handle.log.lock().unwrap();
			log.session = crate::core::session_manager::SessionManager::with_file(
				root.clone(),
				std::fs::File::open(&readonly_path).unwrap(),
			);
		}
		let mut session = RpcSession::new(handle, registry);
		session.handle_line(r#"{"type":"prompt","message":"keep me"}"#);
		until_agent_end(&mut session).await;
		let frame = replacement_response(
			&mut session,
			&json!({"type":"switch_session", "id":"failure", "sessionPath":target_path})
				.to_string(),
		)
		.await;
		assert_eq!(frame["id"], "failure");
		assert_eq!(frame["success"], false);
		assert!(
			frame["error"]
				.as_str()
				.unwrap()
				.contains("could not be saved")
		);
		assert!(session.agent().snapshot().messages.iter().any(
			|message| matches!(message, pi_ai::Message::User(user) if user.content.as_text() == "keep me")
		));
		session.shutdown_settled().await;
		std::fs::remove_dir_all(root).unwrap();
	}

	#[tokio::test]
	async fn replacement_response_survives_wire_event_overflow() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		assert!(
			session
				.handle_line(r#"{"type":"new_session","id":"overflow"}"#)
				.is_empty()
		);
		for _ in 0..2048 {
			let _ = session
				.shared
				.wire_events
				.send(WireEvent::SessionInfoChanged { name: None });
		}
		let response = tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let frame = parse_frame(&session.next_event_frame().await.unwrap());
				if frame["id"] == "overflow" {
					break frame;
				}
			}
		})
		.await
		.unwrap();
		assert_eq!(response["success"], true);
		assert_eq!(
			session
				.pending_replacements
				.load(std::sync::atomic::Ordering::Relaxed),
			0
		);
		session.shutdown_settled().await;
	}

	#[tokio::test]
	async fn replacement_reports_session_task_stopping_before_execution() {
		let (handle, registry) = mock_handle();
		let mut session = RpcSession::new(handle, registry);
		session.cmd_tx.send(SessionCommand::Exit).unwrap();
		let response =
			replacement_response(&mut session, r#"{"type":"new_session","id":"stopped"}"#).await;
		assert_eq!(response["success"], false);
		assert!(response["error"].as_str().unwrap().contains("session task"));
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

	#[tokio::test]
	async fn compact_accepts_the_upstream_custom_instructions_field() {
		let (handle, registry) = mock_handle();
		let session = RpcSession::new(handle, registry);
		let frames = session
			.handle_line(r#"{"type":"compact","id":"c1","customInstructions":"keep paths"}"#);
		assert_eq!(frames.len(), 1);
		let frame: Value = serde_json::from_str(&frames[0]).unwrap();
		assert_eq!(frame["command"], "compact");
		assert_eq!(frame["success"], true);
		assert_eq!(frame["data"]["disposition"], "started");
	}
}
