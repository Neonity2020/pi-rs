//! Low-level agent loop — port of `pi/packages/agent/src/agent-loop.ts`
//! (`runAgentLoop` / `runAgentLoopContinue`).
//!
//! One iteration of the inner loop is one *turn*: prepare → provider stream →
//! assistant message → tool execution → steering poll → follow-up poll. The
//! loop exits on hard stops (`error`/`aborted`), a `terminate` batch, or when
//! no steering/follow-up messages remain after a tool-free turn.
//!
//! Framework-stage simplifications (marked TODO against the TS source):
//! - `tool_execution` always runs sequentially; the parallel preflight/execute
//!   split from `agent-loop.ts` lands with the tokio JoinSet port.
//! - `prepare_request`, `finish_turn`, and `prepare_next_turn` hook invocation
//!   points are not wired yet (hook types are defined in `types.rs`).

use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use pi_ai::{
	AssistantMessage, Message, Model, StopReason, ThinkingLevel, ToolResultMessage,
	TranscriptContext,
};
use pi_ai::{AssistantMessageEvent, StreamFn, StreamOptions};

use crate::types::{
	assistant_tool_calls, is_hard_exit, AgentEvent, AgentTool, AgentToolResult,
	PendingMessageQueue, ToolUpdate,
};

/// Converts agent messages to LLM-compatible messages before each call.
/// `None` keeps the default: pass every standard role through.
pub type ConvertToLlmFn = Arc<dyn Fn(&[Message]) -> Vec<Message> + Send + Sync>;

/// Everything the loop needs for one run. Built by [`crate::Agent`] from its
/// current state; the shared handles keep the agent's live view in sync while
/// the loop runs.
pub struct AgentLoopRun {
	pub model: Model,
	pub thinking_level: ThinkingLevel,
	pub tools: Vec<Arc<dyn AgentTool>>,
	pub stream_fn: StreamFn,
	pub session_id: Option<String>,

	/// Transcript shared with the agent state; the loop appends here.
	pub messages: Arc<RwLock<Vec<Message>>>,
	/// Live partial assistant message, `None` between turns.
	pub streaming: Arc<RwLock<Option<Message>>>,
	pub steering: Arc<Mutex<PendingMessageQueue>>,
	pub follow_up: Arc<Mutex<PendingMessageQueue>>,
	pub event_tx: broadcast::Sender<AgentEvent>,
	pub convert_to_llm: Option<ConvertToLlmFn>,
}

/// Per-invocation options.
pub struct LoopOptions {
	pub signal: CancellationToken,
	/// Suppress the first steering poll — used by `Agent::continue_()` when it
	/// already drained steering itself (mirrors `skipInitialSteeringPoll`).
	pub skip_initial_steering_poll: bool,
}

impl Default for LoopOptions {
	fn default() -> Self {
		Self { signal: CancellationToken::new(), skip_initial_steering_poll: false }
	}
}

/// Serialized summary of a tool result for `tool_execution_end` events.
fn result_summary(result: &AgentToolResult) -> serde_json::Value {
	serde_json::json!({
		"isError": result.is_error,
		"terminate": result.terminate,
		"content": result
			.content
			.iter()
			.filter_map(|c| c.text_content())
			.collect::<Vec<_>>(),
		"details": result.details.clone().unwrap_or(serde_json::Value::Null),
	})
}

fn emit(tx: &broadcast::Sender<AgentEvent>, event: AgentEvent) {
	// No receivers is fine — the loop must not depend on UI presence.
	let _ = tx.send(event);
}

fn append_with_events(run: &AgentLoopRun, message: Message) {
	{
		let mut messages = run.messages.write().unwrap();
		messages.push(message.clone());
	}
	emit(&run.event_tx, AgentEvent::MessageStart { message: message.clone() });
	emit(&run.event_tx, AgentEvent::MessageEnd { message });
}

/// Run the agent loop to completion. Appends `prompt_messages` to the
/// transcript (with lifecycle events), then streams turns until the exit
/// conditions above. Emits `AgentStart` first and `AgentEnd` last.
pub async fn run_agent_loop(
	run: &AgentLoopRun,
	prompt_messages: Vec<Message>,
	opts: LoopOptions,
) -> anyhow::Result<()> {
	let start_len = run.messages.read().unwrap().len();

	emit(&run.event_tx, AgentEvent::AgentStart);
	for message in prompt_messages {
		append_with_events(run, message);
	}

	let mut skip_initial_steering_poll = opts.skip_initial_steering_poll;
	let signal = opts.signal;

	loop {
		if signal.is_cancelled() {
			break;
		}

		emit(&run.event_tx, AgentEvent::TurnStart);

		// TODO(pi): prepare_request hook runs here (context/model/thinking swap).

		let transcript = {
			let messages = run.messages.read().unwrap();
			match &run.convert_to_llm {
				Some(convert) => convert(&messages),
				None => messages.clone(),
			}
		};

		let reasoning =
			if run.thinking_level == ThinkingLevel::Off { None } else { Some(run.thinking_level) };
		let stream_options = StreamOptions {
			api_key: None, // resolved by the stream fn via provider config
			temperature: None,
			max_tokens: None,
			reasoning,
			signal: signal.clone(),
			session_id: run.session_id.clone(),
		};

		let context = TranscriptContext { messages: transcript };
		let mut stream = (run.stream_fn)(&run.model, &context, stream_options);

		let mut final_message: Option<AssistantMessage> = None;
		while let Some(event) = stream.next().await {
			match event {
				AssistantMessageEvent::Start { partial } => {
					let message = Message::Assistant(partial);
					*run.streaming.write().unwrap() = Some(message.clone());
					emit(&run.event_tx, AgentEvent::MessageStart { message });
				}
				AssistantMessageEvent::TextStart { .. }
				| AssistantMessageEvent::ThinkingStart { .. }
				| AssistantMessageEvent::ToolCallStart { .. } => {
					let message = Message::Assistant(event.partial().clone());
					*run.streaming.write().unwrap() = Some(message.clone());
					emit(&run.event_tx, AgentEvent::MessageUpdate { message });
				}
				AssistantMessageEvent::TextDelta { .. }
				| AssistantMessageEvent::ThinkingDelta { .. }
				| AssistantMessageEvent::ToolCallDelta { .. }
				| AssistantMessageEvent::TextEnd { .. }
				| AssistantMessageEvent::ThinkingEnd { .. }
				| AssistantMessageEvent::ToolCallEnd { .. } => {
					let message = Message::Assistant(event.partial().clone());
					*run.streaming.write().unwrap() = Some(message.clone());
					emit(&run.event_tx, AgentEvent::MessageUpdate { message });
				}
				AssistantMessageEvent::Done { message, .. } => {
					final_message = Some(message);
				}
				AssistantMessageEvent::Error { error, .. } => {
					final_message = Some(error);
				}
			}
		}

		let assistant = final_message.unwrap_or_else(|| {
			// Protocol violation: stream ended without Done/Error.
			AssistantMessage::error(&run.model, StopReason::Error, "stream ended without a terminal event")
		});
		*run.streaming.write().unwrap() = None;

		let hard_exit = is_hard_exit(assistant.stop_reason);
		let has_tool_calls = assistant.stop_reason == StopReason::ToolUse;

		{
			let message = Message::Assistant(assistant.clone());
			run.messages.write().unwrap().push(message.clone());
			emit(&run.event_tx, AgentEvent::MessageEnd { message });
		}

		// --- tool execution phase -------------------------------------------
		let mut tool_results: Vec<ToolResultMessage> = Vec::new();
		let mut terminate_batch = false;

		if has_tool_calls {
			let tool_calls: Vec<_> = assistant_tool_calls(&assistant).into_iter().cloned().collect();
			for tool_call in &tool_calls {
				let started = Instant::now();
				emit(
					&run.event_tx,
					AgentEvent::ToolExecutionStart {
						tool_call_id: tool_call.id.clone(),
						tool_name: tool_call.name.clone(),
						args: tool_call.arguments.clone(),
					},
				);

				let tool = run.tools.iter().find(|t| t.name() == tool_call.name);
				let result = match tool {
					Some(tool) => {
						let on_update: Option<ToolUpdate> = None; // wired with streaming tool output
						tool.execute(&tool_call.id, tool_call.arguments.clone(), signal.clone(), on_update)
							.await
					}
					None => AgentToolResult::error(format!(
						"unknown tool: {} (not in the active tool set)",
						tool_call.name
					)),
				};

				let duration_ms = started.elapsed().as_millis() as u64;
				emit(
					&run.event_tx,
					AgentEvent::ToolExecutionEnd {
						tool_call_id: tool_call.id.clone(),
						tool_name: tool_call.name.clone(),
						result: result_summary(&result),
						is_error: result.is_error,
						duration_ms: Some(duration_ms),
					},
				);

				// TODO(pi): after_tool_call hook (content/isError overrides) runs here.

				terminate_batch |= result.terminate;
				let result_message = ToolResultMessage {
					tool_call_id: tool_call.id.clone(),
					tool_name: tool_call.name.clone(),
					content: result.content.clone(),
					details: result.details.clone(),
					usage: result.usage,
					is_error: result.is_error,
					timestamp: pi_ai::types::now_ms(),
					duration_ms: Some(duration_ms),
				};
				let message = Message::ToolResult(result_message.clone());
				run.messages.write().unwrap().push(message.clone());
				emit(&run.event_tx, AgentEvent::MessageStart { message: message.clone() });
				emit(&run.event_tx, AgentEvent::MessageEnd { message });
				tool_results.push(result_message);
			}
		}

		emit(
			&run.event_tx,
			AgentEvent::TurnEnd {
				message: Message::Assistant(assistant.clone()),
				tool_results,
			},
		);

		if hard_exit || terminate_batch {
			break;
		}

		// --- steering poll ---------------------------------------------------
		let steering: Vec<Message> = if skip_initial_steering_poll {
			skip_initial_steering_poll = false;
			Vec::new()
		} else {
			run.steering.lock().unwrap().drain()
		};
		if !steering.is_empty() {
			for message in steering {
				append_with_events(run, message);
			}
			continue;
		}

		// TODO(pi): finish_turn hook runs here ({End} breaks, {Continue}
		// guarantees one more provider request).

		// --- follow-up poll --------------------------------------------------
		if !has_tool_calls {
			let follow_ups = run.follow_up.lock().unwrap().drain();
			if follow_ups.is_empty() {
				break;
			}
			for message in follow_ups {
				append_with_events(run, message);
			}
		}
	}

	let messages: Vec<Message> = run.messages.read().unwrap()[start_len..].to_vec();
	emit(&run.event_tx, AgentEvent::AgentEnd { messages });
	Ok(())
}
