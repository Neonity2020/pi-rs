//! Low-level agent loop — port of `pi/packages/agent/src/agent-loop.ts`
//! (`runAgentLoop` / `runAgentLoopContinue`).
//!
//! One iteration of the inner loop is one *turn*: prepare → provider stream →
//! assistant message → tool execution → steering poll → finish-turn decision →
//! follow-up poll. The loop exits on hard stops (`error`/`aborted`), a
//! `terminate` batch, or when nothing schedules a next request after a
//! tool-free turn (no steering, no follow-ups, no explicit continuation).
//!
//! Deliberate deviations from the TS source (marked against it):
//! - Tool-call execution runs in a single loop rather than the nested
//!   inner/outer structure; the `explicitContinuation` semantics are kept.
//! - `finish_turn` runs after the steering poll (upstream: before it), so a
//!   turn that injected steering does not reach the hook. Hard exits and
//!   terminate batches break before the hook point.
//! - In parallel mode a `before_tool_call` block carrying `terminate: true`
//!   stops launching subsequent calls of the batch (they receive error
//!   results); upstream only stops the preflight on the abort signal.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use pi_ai::{
	AssistantMessage, Message, Model, StopReason, ThinkingLevel, ToolCall, ToolResultMessage,
	TranscriptContext,
};
use pi_ai::{AssistantMessageEvent, StreamFn, StreamOptions};

use crate::types::{
	AfterToolCallContext, AfterToolCallResult, AgentEvent, AgentTool, AgentToolResult,
	AgentTurnDecision, BeforeToolCallContext, BeforeToolCallResult, PendingMessageQueue,
	ToolExecutionMode, ToolUpdate, assistant_tool_calls, is_hard_exit,
};

/// Converts agent messages to LLM-compatible messages before each call.
/// `None` keeps the default: pass every standard role through.
pub type ConvertToLlmFn = Arc<dyn Fn(&[Message]) -> Vec<Message> + Send + Sync>;

/// Replacement state returned by the `prepare_next_turn` hook (upstream
/// `AgentLoopTurnUpdate` minus context-object semantics: the transcript IS
/// the context here). `model`/`thinking_level` swap the next request's
/// config; `messages` are appended with normal lifecycle events before it.
#[derive(Default)]
pub struct AgentTurnUpdate {
	pub messages: Vec<Message>,
	pub model: Option<Model>,
	pub thinking_level: Option<ThinkingLevel>,
}

/// Called after `turn_end` when the loop will continue, before the next
/// request. Receives the current transcript. Async by design: long-running
/// work is expected (the port runs mid-run compaction behind this hook), so
/// the steering queue is polled again right after the hook resolves.
pub type PrepareNextTurnFn = Arc<
	dyn Fn(Vec<Message>) -> Pin<Box<dyn Future<Output = Option<AgentTurnUpdate>> + Send>>
		+ Send
		+ Sync,
>;

/// Replacement state for the request being prepared (upstream
/// `AgentRequestUpdate`): swaps the model/thinking level of the request that is
/// about to be sent. The transcript is the context and is not replaceable here.
#[derive(Default)]
pub struct AgentRequestUpdate {
	pub model: Option<Model>,
	pub thinking_level: Option<ThinkingLevel>,
}

/// Runtime state handed to the `prepare_request` hook immediately before every
/// provider request, including the first (upstream `PrepareRequestContext`).
/// Pending messages have already been appended when the hook runs.
pub struct PrepareRequestContext {
	pub messages: Vec<Message>,
	pub model: Model,
	pub thinking_level: ThinkingLevel,
}

/// Called immediately before every provider request. Async by design,
/// mirroring the `prepare_next_turn` style.
pub type PrepareRequestFn = Arc<
	dyn Fn(
			PrepareRequestContext,
		) -> Pin<Box<dyn Future<Output = Option<AgentRequestUpdate>> + Send>>
		+ Send
		+ Sync,
>;

/// Completed-turn context passed to the `finish_turn` hook (upstream
/// `AgentTurnContext`): the assistant message that completed the turn, its
/// tool results, and the transcript after they were appended.
pub struct AgentTurnContext {
	pub message: AssistantMessage,
	pub tool_results: Vec<ToolResultMessage>,
	pub messages: Vec<Message>,
}

/// Called after the steering poll, before the follow-up poll. `End` breaks the
/// loop even when follow-up messages are queued; `Continue` guarantees one more
/// provider request even when nothing else schedules one (upstream
/// `explicitContinuation`). `None` preserves normal scheduling.
pub type FinishTurnFn = Arc<
	dyn Fn(AgentTurnContext) -> Pin<Box<dyn Future<Output = Option<AgentTurnDecision>> + Send>>
		+ Send
		+ Sync,
>;

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
	pub prepare_next_turn: Option<PrepareNextTurnFn>,
	/// Batch execution strategy; a tool declaring `sequential` forces the
	/// sequential path for its whole batch.
	pub tool_execution: ToolExecutionMode,
	pub prepare_request: Option<PrepareRequestFn>,
	pub finish_turn: Option<FinishTurnFn>,
	pub before_tool_call: Option<crate::types::BeforeToolCallFn>,
	pub after_tool_call: Option<crate::types::AfterToolCallFn>,
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
		Self {
			signal: CancellationToken::new(),
			skip_initial_steering_poll: false,
		}
	}
}

/// Outcome of one tool call after hooks ran — mirrors upstream
/// `AgentToolCallOutcome`. `duration_ms` is `None` when the tool did not run.
struct FinalizedToolCall {
	tool_call: ToolCall,
	result: AgentToolResult,
	is_error: bool,
	duration_ms: Option<u64>,
}

/// Tool results of one assistant message's tool-call batch, mirroring the
/// upstream `ExecutedToolCallBatch`. The batch `terminate` flag ends the run
/// after the turn.
struct ExecutedToolBatch {
	messages: Vec<ToolResultMessage>,
	terminate: bool,
}

/// Serialized summary of a tool result for `tool_execution_end` events.
fn result_summary(result: &AgentToolResult) -> serde_json::Value {
	let mut value = serde_json::json!({"content": result.content});
	let object = value.as_object_mut().expect("object");
	if let Some(details) = &result.details {
		object.insert("details".into(), details.clone());
	}
	if let Some(content) = &result.structured_content {
		object.insert("structuredContent".into(), content.clone());
	}
	if let Some(usage) = &result.usage {
		object.insert("usage".into(), serde_json::json!(usage));
	}
	if result.is_error {
		object.insert("isError".into(), true.into());
	}
	if result.terminate {
		object.insert("terminate".into(), true.into());
	}
	value
}

fn emit(tx: &broadcast::Sender<AgentEvent>, event: AgentEvent) {
	// No receivers is fine — the loop must not depend on UI presence.
	let _ = tx.send(event);
}

fn append_with_events(run: &AgentLoopRun, message: Message) {
	push_message(run, message.clone());
	emit(
		&run.event_tx,
		AgentEvent::MessageStart {
			message: message.clone(),
		},
	);
	emit(&run.event_tx, AgentEvent::MessageEnd { message });
}

/// Push into the transcript without lifecycle events — used when the
/// announcement is deferred past the next `turn_start` (queued messages).
fn push_message(run: &AgentLoopRun, message: Message) {
	let mut messages = run.messages.write().unwrap();
	messages.push(message);
}

/// Run the agent loop to completion. Appends `prompt_messages` to the
/// transcript (with lifecycle events), then streams turns until the exit
/// conditions above. Emits `AgentStart` first and `AgentEnd` last.
fn assistant_event_json(event: &AssistantMessageEvent) -> serde_json::Value {
	use AssistantMessageEvent as E;
	let mut value = match event {
		E::TextStart { content_index, .. } => {
			serde_json::json!({"type":"text_start", "contentIndex": content_index})
		}
		E::TextDelta {
			content_index,
			delta,
			..
		} => serde_json::json!({"type":"text_delta", "contentIndex": content_index, "delta":delta}),
		E::TextEnd {
			content_index,
			content,
			..
		} => {
			serde_json::json!({"type":"text_end", "contentIndex": content_index, "content":content})
		}
		E::ThinkingStart { content_index, .. } => {
			serde_json::json!({"type":"thinking_start", "contentIndex": content_index})
		}
		E::ThinkingDelta {
			content_index,
			delta,
			..
		} => {
			serde_json::json!({"type":"thinking_delta", "contentIndex": content_index, "delta":delta})
		}
		E::ThinkingEnd {
			content_index,
			content,
			..
		} => {
			serde_json::json!({"type":"thinking_end", "contentIndex": content_index, "content":content})
		}
		E::ToolCallStart { content_index, .. } => {
			serde_json::json!({"type":"toolcall_start", "contentIndex": content_index})
		}
		E::ToolCallDelta {
			content_index,
			delta,
			..
		} => {
			serde_json::json!({"type":"toolcall_delta", "contentIndex": content_index, "delta":delta})
		}
		E::ToolCallEnd {
			content_index,
			tool_call,
			..
		} => {
			// Upstream wires the tool call as a tagged content block
			// (`{"type":"toolCall",...}`, agent-loop.ts spreads the block).
			serde_json::json!({"type":"toolcall_end", "contentIndex": content_index, "toolCall":pi_ai::AssistantContent::ToolCall(tool_call.clone())})
		}
		_ => unreachable!("only non-terminal update events are forwarded"),
	};
	value["partial"] = serde_json::json!(event.partial());
	value
}

pub async fn run_agent_loop(
	run: &mut AgentLoopRun,
	prompt_messages: Vec<Message>,
	opts: LoopOptions,
) -> anyhow::Result<()> {
	let start_len = run.messages.read().unwrap().len();

	emit(&run.event_tx, AgentEvent::AgentStart);
	// Upstream agent-loop.ts:126-133: the entry function emits the first
	// `turn_start` and THEN announces each prompt message; continuation turns
	// re-emit `turn_start` inside runLoop (agent-loop.ts:216) before the
	// queued messages are announced (agent-loop.ts:221-227). pi-rs used to
	// announce prompt messages before `turn_start`, which the JSON/RPC event
	// diff pinned as an ordering divergence.
	emit(&run.event_tx, AgentEvent::TurnStart);
	for message in prompt_messages {
		append_with_events(run, message);
	}
	let mut first_turn = true;
	// Messages drained from the steering/follow-up queues in the previous
	// iteration; announced after the continuation `turn_start`, like
	// upstream's `pendingMessages` (agent-loop.ts:217-227).
	let mut queued: Vec<Message> = Vec::new();

	let mut skip_initial_steering_poll = opts.skip_initial_steering_poll;
	let signal = opts.signal;
	// Upstream `explicitContinuation`: a `finish_turn` Continue decision
	// guarantees one more provider request even when no tool calls, steering,
	// or follow-up messages schedule one.
	let mut explicit_continuation = false;

	loop {
		if first_turn {
			// `agent_start` + first `turn_start` + prompt announcements
			// already happened before the loop (upstream entry function).
			first_turn = false;
		} else {
			emit(&run.event_tx, AgentEvent::TurnStart);
			// Queued messages were already pushed into the transcript when
			// their queue drained (so a mid-run compaction swap keeps them);
			// only their announcement waits for this `turn_start`, matching
			// upstream's announce-after-turn_start (agent-loop.ts:221-227).
			for message in std::mem::take(&mut queued) {
				emit(
					&run.event_tx,
					AgentEvent::MessageStart {
						message: message.clone(),
					},
				);
				emit(&run.event_tx, AgentEvent::MessageEnd { message });
			}
		}
		if signal.is_cancelled() {
			break;
		}

		// --- prepare_request hook (upstream: pending messages are already
		// appended; may swap model/thinking for this and later requests) -----
		if let Some(hook) = &run.prepare_request {
			let request = PrepareRequestContext {
				messages: run.messages.read().unwrap().clone(),
				model: run.model.clone(),
				thinking_level: run.thinking_level,
			};
			if let Some(update) = hook(request).await {
				if let Some(model) = update.model {
					run.model = model;
				}
				if let Some(level) = update.thinking_level {
					run.thinking_level = level;
				}
			}
		}

		let transcript = {
			let messages = run.messages.read().unwrap();
			match &run.convert_to_llm {
				Some(convert) => convert(&messages),
				None => messages.clone(),
			}
		};

		let reasoning = if run.thinking_level == ThinkingLevel::Off {
			None
		} else {
			Some(run.thinking_level)
		};
		let stream_options = StreamOptions {
			api_key: None, // resolved by the stream fn via provider config
			temperature: None,
			max_tokens: None,
			reasoning,
			signal: signal.clone(),
			session_id: run.session_id.clone(),
		};

		let context = TranscriptContext {
			messages: transcript,
		};
		// The upstream AssistantMessageEventStream stamps `durationMs` on the
		// final message with a monotonic clock taken at stream creation.
		let turn_started = Instant::now();
		let turn_started_ms = pi_ai::types::now_ms();
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
					emit(
						&run.event_tx,
						AgentEvent::MessageUpdate {
							message,
							assistant_message_event: assistant_event_json(&event),
						},
					);
				}
				AssistantMessageEvent::TextDelta { .. }
				| AssistantMessageEvent::ThinkingDelta { .. }
				| AssistantMessageEvent::ToolCallDelta { .. }
				| AssistantMessageEvent::TextEnd { .. }
				| AssistantMessageEvent::ThinkingEnd { .. }
				| AssistantMessageEvent::ToolCallEnd { .. } => {
					let message = Message::Assistant(event.partial().clone());
					*run.streaming.write().unwrap() = Some(message.clone());
					emit(
						&run.event_tx,
						AgentEvent::MessageUpdate {
							message,
							assistant_message_event: assistant_event_json(&event),
						},
					);
				}
				AssistantMessageEvent::Done { message, .. } => {
					final_message = Some(message);
				}
				AssistantMessageEvent::Error { error, .. } => {
					final_message = Some(error);
				}
			}
		}

		let mut assistant = final_message.unwrap_or_else(|| {
			// Protocol violation: stream ended without Done/Error.
			AssistantMessage::error(
				&run.model,
				StopReason::Error,
				"stream ended without a terminal event",
			)
		});
		// Monotonic stamp (upstream event-stream.ts): skip messages that already
		// carry a duration or predate the stream (deferred results).
		if assistant.duration_ms.is_none() && assistant.timestamp >= turn_started_ms {
			assistant.duration_ms = Some(turn_started.elapsed().as_millis() as u64);
		}
		// Upstream stamps the requested level on the final message
		// (agent-loop.ts:419 `thinkingLevel: config.reasoning ?? "off"`).
		assistant.thinking_level = Some(run.thinking_level);
		*run.streaming.write().unwrap() = None;

		let hard_exit = is_hard_exit(assistant.stop_reason);
		// Tool calls are detected by filtering content blocks
		// (agent-loop.ts:268), not by the stop reason: a message that stops
		// with `stop` but carries tool-call blocks still executes them.
		let tool_calls: Vec<ToolCall> = assistant_tool_calls(&assistant)
			.into_iter()
			.cloned()
			.collect();
		let has_tool_calls = !tool_calls.is_empty();

		{
			let message = Message::Assistant(assistant.clone());
			run.messages.write().unwrap().push(message.clone());
			emit(&run.event_tx, AgentEvent::MessageEnd { message });
		}

		// --- tool execution phase -------------------------------------------
		let mut tool_results: Vec<ToolResultMessage> = Vec::new();
		let mut terminate_batch = false;

		if !hard_exit && has_tool_calls {
			// A `length` stop means the output was cut off by the token limit,
			// so every tool call may carry truncated arguments; fail the whole
			// batch instead of executing it (upstream
			// `failToolCallsFromTruncatedMessage`).
			let batch = if assistant.stop_reason == StopReason::Length {
				fail_tool_calls_from_truncated_message(run, &tool_calls)
			} else {
				// A tool declaring `sequential` forces the sequential path for
				// its whole batch (upstream `hasSequentialToolCall`).
				let sequential = run.tool_execution == ToolExecutionMode::Sequential
					|| tool_calls.iter().any(|call| {
						run.tools
							.iter()
							.find(|tool| tool.name() == call.name)
							.is_some_and(|tool| {
								tool.execution_mode() == Some(ToolExecutionMode::Sequential)
							})
					});
				let hook_messages = run.messages.read().unwrap().clone();
				if sequential {
					execute_tool_calls_sequential(
						run,
						&assistant,
						&tool_calls,
						&signal,
						&hook_messages,
					)
					.await
				} else {
					execute_tool_calls_parallel(
						run,
						&assistant,
						&tool_calls,
						&signal,
						&hook_messages,
					)
					.await
				}
			};

			terminate_batch = batch.terminate;
			tool_results = batch.messages;
		}

		emit(
			&run.event_tx,
			AgentEvent::TurnEnd {
				message: Message::Assistant(assistant.clone()),
				tool_results: tool_results
					.iter()
					.map(|result| Message::ToolResult(result.clone()))
					.collect(),
			},
		);

		if hard_exit || terminate_batch {
			break;
		}

		// --- prepare_next_turn hook (upstream: after turn_end, before the next
		// request; may swap model/thinking and append messages) ---------------
		if let Some(hook) = &run.prepare_next_turn {
			let messages = run.messages.read().unwrap().clone();
			let update = hook(messages).await;
			if let Some(update) = update {
				if let Some(model) = update.model {
					run.model = model;
				}
				if let Some(level) = update.thinking_level {
					run.thinking_level = level;
				}
				for message in update.messages {
					append_with_events(run, message);
				}
			}
		}

		// --- steering poll ---------------------------------------------------
		let steering: Vec<Message> = if skip_initial_steering_poll {
			skip_initial_steering_poll = false;
			Vec::new()
		} else {
			run.steering.lock().unwrap().drain()
		};
		if !steering.is_empty() {
			// Push now — a mid-run compaction swap keeps the transcript
			// suffix, so deferring the push would drop the message on the
			// hook's abort — but announce after the next `turn_start`
			// (upstream agent-loop.ts:217-227).
			for message in steering {
				push_message(run, message.clone());
				queued.push(message);
			}
			// Injected steering already satisfies the next request.
			explicit_continuation = false;
			continue;
		}

		// --- finish_turn hook ({End} breaks even with queued follow-ups;
		// {Continue} guarantees one more provider request) --------------------
		if let Some(hook) = &run.finish_turn {
			let turn = AgentTurnContext {
				message: assistant.clone(),
				tool_results: tool_results.clone(),
				messages: run.messages.read().unwrap().clone(),
			};
			match hook(turn).await {
				Some(AgentTurnDecision::End) => break,
				Some(AgentTurnDecision::Continue) => explicit_continuation = true,
				None => {}
			}
		}
		// Tool calls already guarantee a follow-up request; the explicit
		// continuation only matters for tool-free turns.
		if has_tool_calls {
			explicit_continuation = false;
		}

		// --- follow-up poll --------------------------------------------------
		if !has_tool_calls {
			let follow_ups = run.follow_up.lock().unwrap().drain();
			if follow_ups.is_empty() {
				if explicit_continuation {
					// No natural request was selected, so fulfill the
					// continuation decision with one context-only turn.
					explicit_continuation = false;
					continue;
				}
				break;
			}
			explicit_continuation = false;
			for message in follow_ups {
				push_message(run, message.clone());
				queued.push(message);
			}
		}
	}

	let messages: Vec<Message> = run.messages.read().unwrap()[start_len..].to_vec();
	emit(&run.event_tx, AgentEvent::AgentEnd { messages });
	Ok(())
}

// --- tool execution ----------------------------------------------------------

fn emit_tool_execution_start(run: &AgentLoopRun, tool_call: &ToolCall) {
	emit(
		&run.event_tx,
		AgentEvent::ToolExecutionStart {
			tool_call_id: tool_call.id.clone(),
			tool_name: tool_call.name.clone(),
			args: tool_call.arguments.clone(),
		},
	);
}

fn emit_tool_execution_end(run: &AgentLoopRun, entry: &FinalizedToolCall) {
	emit(
		&run.event_tx,
		AgentEvent::ToolExecutionEnd {
			tool_call_id: entry.tool_call.id.clone(),
			tool_name: entry.tool_call.name.clone(),
			result: result_summary(&entry.result),
			is_error: entry.is_error,
			duration_ms: entry.duration_ms,
		},
	);
}

/// Append a tool result message to the transcript with its lifecycle events.
fn append_tool_result(run: &AgentLoopRun, entry: &FinalizedToolCall) -> ToolResultMessage {
	let result_message = ToolResultMessage {
		tool_call_id: entry.tool_call.id.clone(),
		tool_name: entry.tool_call.name.clone(),
		content: entry.result.content.clone(),
		details: entry.result.details.clone(),
		usage: entry.result.usage,
		is_error: entry.is_error,
		timestamp: pi_ai::types::now_ms(),
		duration_ms: entry.duration_ms,
	};
	let message = Message::ToolResult(result_message.clone());
	run.messages.write().unwrap().push(message.clone());
	emit(
		&run.event_tx,
		AgentEvent::MessageStart {
			message: message.clone(),
		},
	);
	emit(&run.event_tx, AgentEvent::MessageEnd { message });
	result_message
}

/// Unknown-tool outcome — the tool never runs, so no duration is recorded.
fn unknown_tool_entry(tool_call: &ToolCall) -> FinalizedToolCall {
	FinalizedToolCall {
		tool_call: tool_call.clone(),
		result: AgentToolResult::error(format!(
			"unknown tool: {} (not in the active tool set)",
			tool_call.name
		)),
		is_error: true,
		duration_ms: None,
	}
}

/// Turns a `before_tool_call` decision into a blocking error result
/// (upstream `prepareToolCall`): `None` when the call may proceed.
fn blocked_result(decision: BeforeToolCallResult) -> Option<AgentToolResult> {
	if !decision.block {
		return None;
	}
	let mut result = AgentToolResult::error(
		decision
			.reason
			.unwrap_or_else(|| "Tool execution was blocked".into()),
	);
	result.terminate = decision.terminate;
	Some(result)
}

/// Runs the `before_tool_call` hook, if installed and the tool exists
/// (upstream runs it after tool resolution, so unknown tools skip it).
fn run_before_hook(
	run: &AgentLoopRun,
	assistant: &AssistantMessage,
	tool_call: &ToolCall,
	hook_messages: &[Message],
) -> Option<AgentToolResult> {
	let hook = run.before_tool_call.as_ref()?;
	run.tools
		.iter()
		.find(|tool| tool.name() == tool_call.name)?;
	let decision = hook(BeforeToolCallContext {
		assistant_message: assistant,
		tool_call,
		messages: hook_messages,
	});
	blocked_result(decision)
}

/// Runs the `after_tool_call` hook and applies its field-level overrides onto
/// the executed result (upstream `finalizeExecutedToolCall`).
fn apply_after_hook(
	run: &AgentLoopRun,
	assistant: &AssistantMessage,
	tool_call: &ToolCall,
	result: &mut AgentToolResult,
	is_error: &mut bool,
	hook_messages: &[Message],
) {
	let Some(hook) = run.after_tool_call.as_ref() else {
		return;
	};
	let after: AfterToolCallResult = hook(AfterToolCallContext {
		assistant_message: assistant,
		tool_call,
		result,
		is_error: *is_error,
		messages: hook_messages,
	});
	// Structured content not replaced along with the content may no longer
	// match it, so it is dropped when only the content is overridden.
	let structured_content = match after.structured_content {
		Some(structured) => Some(structured),
		None if after.content.is_some() => None,
		None => result.structured_content.clone(),
	};
	if let Some(content) = after.content {
		result.content = content;
	}
	if let Some(details) = after.details {
		result.details = Some(details);
	}
	if let Some(usage) = after.usage {
		result.usage = Some(usage);
	}
	if let Some(terminate) = after.terminate {
		result.terminate = terminate;
	}
	result.structured_content = structured_content;
	if let Some(error) = after.is_error {
		result.is_error = error;
		*is_error = error;
	}
}

/// The partial-update callback handed to `execute`. A retained callback must
/// not emit after execute has settled; the flag also orders an in-flight
/// update before `tool_execution_end`.
fn tool_update_callback(
	run: &AgentLoopRun,
	tool_call: &ToolCall,
	accepting: Arc<Mutex<bool>>,
) -> ToolUpdate {
	let tool_call_id = tool_call.id.clone();
	let tool_name = tool_call.name.clone();
	let args = tool_call.arguments.clone();
	let event_tx = run.event_tx.clone();
	Arc::new(move |partial: AgentToolResult| {
		let guard = accepting.lock().unwrap_or_else(|e| e.into_inner());
		if !*guard {
			return;
		}
		let mut result = serde_json::json!({"content": partial.content});
		let object = result.as_object_mut().expect("object");
		if let Some(details) = partial.details {
			object.insert("details".into(), details);
		}
		if let Some(content) = partial.structured_content {
			object.insert("structuredContent".into(), content);
		}
		if let Some(usage) = partial.usage {
			object.insert("usage".into(), serde_json::json!(usage));
		}
		if partial.is_error {
			object.insert("isError".into(), true.into());
		}
		if partial.terminate {
			object.insert("terminate".into(), true.into());
		}
		emit(
			&event_tx,
			AgentEvent::ToolExecutionUpdate {
				tool_call_id: tool_call_id.clone(),
				tool_name: tool_name.clone(),
				args: args.clone(),
				partial_result: result,
			},
		);
	})
}

/// Execute each tool call to completion before starting the next
/// (`executeToolCallsSequential`): start → hooks → execute → end → result
/// message, per call in source order.
async fn execute_tool_calls_sequential(
	run: &AgentLoopRun,
	assistant: &AssistantMessage,
	tool_calls: &[ToolCall],
	signal: &CancellationToken,
	hook_messages: &[Message],
) -> ExecutedToolBatch {
	let mut messages = Vec::new();
	let mut terminate = false;
	for tool_call in tool_calls {
		emit_tool_execution_start(run, tool_call);

		let entry = if let Some(blocked) = run_before_hook(run, assistant, tool_call, hook_messages)
		{
			FinalizedToolCall {
				tool_call: tool_call.clone(),
				is_error: true,
				duration_ms: None,
				result: blocked,
			}
		} else {
			match run.tools.iter().find(|tool| tool.name() == tool_call.name) {
				Some(tool) => {
					let started = Instant::now();
					let accepting = Arc::new(Mutex::new(true));
					let on_update = tool_update_callback(run, tool_call, accepting.clone());
					let mut result = tool
						.execute(
							&tool_call.id,
							tool_call.arguments.clone(),
							signal.clone(),
							Some(on_update),
						)
						.await;
					*accepting.lock().unwrap_or_else(|e| e.into_inner()) = false;
					let mut is_error = result.is_error;
					apply_after_hook(
						run,
						assistant,
						tool_call,
						&mut result,
						&mut is_error,
						hook_messages,
					);
					FinalizedToolCall {
						tool_call: tool_call.clone(),
						result,
						is_error,
						duration_ms: Some(started.elapsed().as_millis() as u64),
					}
				}
				None => unknown_tool_entry(tool_call),
			}
		};

		emit_tool_execution_end(run, &entry);
		messages.push(append_tool_result(run, &entry));
		terminate |= entry.result.terminate;
	}
	ExecutedToolBatch {
		messages,
		terminate,
	}
}

/// Execute tool calls concurrently (`executeToolCallsParallel`): all
/// `tool_execution_start` events are emitted first in source order (unknown
/// tools included, immediately followed by their error end); the
/// `before_tool_call` hook runs in the preflight so blocked calls never start.
/// Prepared calls then execute concurrently on a JoinSet;
/// `tool_execution_end` is emitted in completion order while the
/// `ToolResultMessage`s are appended in source order. Calls already launched
/// always run to completion.
async fn execute_tool_calls_parallel(
	run: &AgentLoopRun,
	assistant: &AssistantMessage,
	tool_calls: &[ToolCall],
	signal: &CancellationToken,
	hook_messages: &[Message],
) -> ExecutedToolBatch {
	let mut finalized: Vec<Option<FinalizedToolCall>> =
		(0..tool_calls.len()).map(|_| None).collect();
	let mut prepared: Vec<(usize, ToolCall, Arc<dyn AgentTool>)> = Vec::new();
	// A preflight-observable terminate (hook block) stops launching subsequent
	// calls of the batch; already launched calls run to completion.
	let mut batch_terminated = false;

	// Preflight (source order).
	for (index, tool_call) in tool_calls.iter().enumerate() {
		emit_tool_execution_start(run, tool_call);
		if let Some(blocked) = run_before_hook(run, assistant, tool_call, hook_messages) {
			let terminated = blocked.terminate;
			let entry = FinalizedToolCall {
				tool_call: tool_call.clone(),
				is_error: true,
				duration_ms: None,
				result: blocked,
			};
			emit_tool_execution_end(run, &entry);
			finalized[index] = Some(entry);
			if terminated {
				batch_terminated = true;
			}
			continue;
		}
		match run.tools.iter().find(|tool| tool.name() == tool_call.name) {
			Some(tool) => prepared.push((index, tool_call.clone(), tool.clone())),
			None => {
				let entry = unknown_tool_entry(tool_call);
				emit_tool_execution_end(run, &entry);
				finalized[index] = Some(entry);
			}
		}
	}

	if batch_terminated {
		for (index, tool_call, _) in prepared.drain(..) {
			let entry = FinalizedToolCall {
				is_error: true,
				duration_ms: None,
				result: AgentToolResult::error(format!(
					"Tool call \"{}\" was not executed: an earlier tool result in this batch requested termination.",
					tool_call.name
				)),
				tool_call,
			};
			emit_tool_execution_end(run, &entry);
			finalized[index] = Some(entry);
		}
	} else {
		let mut join_set: JoinSet<(usize, ToolCall, AgentToolResult, u64)> = JoinSet::new();
		for (index, tool_call, tool) in prepared {
			let task_signal = signal.clone();
			let accepting = Arc::new(Mutex::new(true));
			let on_update = tool_update_callback(run, &tool_call, accepting.clone());
			join_set.spawn(async move {
				let started = Instant::now();
				let result = tool
					.execute(
						&tool_call.id,
						tool_call.arguments.clone(),
						task_signal,
						Some(on_update),
					)
					.await;
				*accepting.lock().unwrap_or_else(|e| e.into_inner()) = false;
				(
					index,
					tool_call,
					result,
					started.elapsed().as_millis() as u64,
				)
			});
		}

		// Completion order: finalize (after hook) and emit the end event as
		// each call settles. A panicked task yields no item; the sweep below
		// records an error outcome for it.
		while let Some(joined) = join_set.join_next().await {
			let Ok((index, tool_call, mut result, duration_ms)) = joined else {
				continue;
			};
			let mut is_error = result.is_error;
			apply_after_hook(
				run,
				assistant,
				&tool_call,
				&mut result,
				&mut is_error,
				hook_messages,
			);
			let entry = FinalizedToolCall {
				tool_call,
				result,
				is_error,
				duration_ms: Some(duration_ms),
			};
			emit_tool_execution_end(run, &entry);
			finalized[index] = Some(entry);
		}
	}

	// Safety net for calls that never completed (panicked task).
	for (index, tool_call) in tool_calls.iter().enumerate() {
		if finalized[index].is_none() {
			let entry = FinalizedToolCall {
				tool_call: tool_call.clone(),
				is_error: true,
				duration_ms: None,
				result: AgentToolResult::error("tool execution did not complete"),
			};
			emit_tool_execution_end(run, &entry);
			finalized[index] = Some(entry);
		}
	}

	// Tool-result message artifacts follow in source order.
	let messages = finalized
		.iter()
		.flatten()
		.map(|entry| append_tool_result(run, entry))
		.collect();
	let terminate = finalized
		.iter()
		.flatten()
		.any(|entry| entry.result.terminate);
	ExecutedToolBatch {
		messages,
		terminate,
	}
}

/// Upstream `failToolCallsFromTruncatedMessage`: a `length` stop means the
/// response hit the output token limit and streamed tool-call arguments may be
/// silently incomplete even when they parse and validate. No call in the batch
/// is executed; each receives an error result so the model can re-issue it.
fn fail_tool_calls_from_truncated_message(
	run: &AgentLoopRun,
	tool_calls: &[ToolCall],
) -> ExecutedToolBatch {
	let mut messages = Vec::new();
	for tool_call in tool_calls {
		emit_tool_execution_start(run, tool_call);
		let entry = FinalizedToolCall {
			tool_call: tool_call.clone(),
			is_error: true,
			duration_ms: None,
			result: AgentToolResult::error(format!(
				"Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
				tool_call.name
			)),
		};
		emit_tool_execution_end(run, &entry);
		messages.push(append_tool_result(run, &entry));
	}
	ExecutedToolBatch {
		messages,
		terminate: false,
	}
}
