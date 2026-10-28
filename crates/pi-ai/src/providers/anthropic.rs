//! Anthropic Messages API adapter — port of the streaming `stream()` in
//! `pi/packages/ai/src/api/anthropic-messages.ts` (via `streamSimple`, whose
//! thinking/budget mapping lives in `pi/packages/ai/src/api/simple-options.ts`).
//!
//! The response body is consumed incrementally: SSE frames (`event:` name plus
//! `data:` payload) are decoded as they arrive and every Anthropic message
//! event (`message_start`, `content_block_start/delta/stop`, `message_delta`,
//! `message_stop`) immediately emits protocol events, so consumers see
//! text/thinking/tool-call updates while the response is still in flight.
//! Tool-call arguments accumulate per Anthropic block index and are parsed
//! once the block stops (`pi/packages/ai/src/utils/streaming-json.ts` and
//! `pi/packages/ai/src/utils/json-parse.ts` semantics — shared with the
//! OpenAI adapter via [`super::json_repair`]). Provider `compat` flags and
//! OAuth/Claude-Code identity flows are not modeled; the defaults upstream
//! uses without them apply.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::json_repair::{lenient_partial_json, parse_json_with_repair, parse_tool_call_arguments};
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, Message, StopReason, SystemContent, TextContent,
	TextOrImage, ThinkingContent, ThinkingLevel, Tool, ToolCall, Usage, UserContent, now_ms,
};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// SSE `event:` names carrying message data; everything else (`ping`,
/// unknown events) is skipped, matching `ANTHROPIC_MESSAGE_EVENTS`.
const MESSAGE_EVENTS: [&str; 6] = [
	"message_start",
	"message_delta",
	"message_stop",
	"content_block_start",
	"content_block_delta",
	"content_block_stop",
];

/// Protocol version pinned by the `@anthropic-ai/sdk` request the TS adapter
/// issues.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Prompt-cache breakpoint added to the system block, the last tool, and the
/// last user block (short retention; long-retention compat is not modeled).
fn cache_control() -> Value {
	json!({"type": "ephemeral"})
}

/// Tokens always left for the answer when a thinking budget shares the
/// response ceiling (`MIN_ANSWER_TOKENS` in simple-options.ts).
const MIN_ANSWER_TOKENS: u64 = 1024;

pub fn stream_fn() -> StreamFn {
	Arc::new(
		|model: &Model,
		 context: &TranscriptContext,
		 options: StreamOptions|
		 -> AssistantMessageEventStream { spawn_stream(model.clone(), context.clone(), options) },
	)
}

/// Runs the request on a task and forwards events through a channel, so the
/// returned stream yields deltas while the HTTP body is still arriving.
fn spawn_stream(
	model: Model,
	context: TranscriptContext,
	options: StreamOptions,
) -> AssistantMessageEventStream {
	let (tx, rx) = tokio::sync::mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
	tokio::spawn(async move {
		let signal = options.signal.clone();
		let result = tokio::select! {
			_ = signal.cancelled() => Err(StreamFailure {
				reason: StopReason::Aborted,
				message: "aborted by user".to_string(),
				session: None,
			}),
			result = run_stream(&model, &context, &options, &tx) => result,
		};
		if let Err(failure) = result {
			let error = match failure.session {
				Some(mut message) => {
					message.stop_reason = failure.reason;
					message.error_message = Some(failure.message);
					message
				}
				None => Box::new(AssistantMessage::error(
					&model,
					failure.reason,
					failure.message,
				)),
			};
			let _ = tx
				.send(Event::Error {
					reason: failure.reason,
					error: *error,
				})
				.await;
		}
	});
	Box::pin(futures::stream::unfold(rx, |mut rx| async move {
		rx.recv().await.map(|event| (event, rx))
	}))
}

/// A terminal stream failure. `session` carries the partially accumulated
/// assistant message when the failure happened mid-stream, mirroring the TS
/// catch block that reports the live output with an error stop reason.
struct StreamFailure {
	reason: StopReason,
	message: String,
	session: Option<Box<AssistantMessage>>,
}

async fn run_stream(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
	tx: &Sender<Event>,
) -> Result<(), StreamFailure> {
	let mut response = open_response(model, context, options)
		.await
		.map_err(|message| StreamFailure {
			reason: StopReason::Error,
			message,
			session: None,
		})?;
	let mut session = StreamSession::new(model);
	if !forward(
		tx,
		vec![Event::Start {
			partial: session.partial(),
		}],
	)
	.await
	{
		return Ok(());
	}

	let mut decoder = SseDecoder::default();
	loop {
		match response.chunk().await {
			Ok(Some(chunk)) => {
				let mut frames = Vec::new();
				decoder.feed(&chunk, &mut frames);
				if !forward_frames(&mut session, frames, tx).await? {
					return Ok(());
				}
			}
			Ok(None) => break,
			Err(error) => {
				return Err(StreamFailure {
					reason: StopReason::Error,
					message: format!("stream read failed: {error}"),
					session: Some(Box::new(session.into_message())),
				});
			}
		}
	}
	// Flush a final unterminated line plus any event still open at EOF.
	let mut frames = Vec::new();
	decoder.finish(&mut frames);
	if !forward_frames(&mut session, frames, tx).await? {
		return Ok(());
	}

	// Tail checks of `iterateAnthropicEvents` and the pre-Done guards.
	if session.saw_message_start && !session.saw_message_stop {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: "Anthropic stream ended before message_stop".to_string(),
			session: Some(Box::new(session.into_message())),
		});
	}
	if session.message.stop_reason == StopReason::Pending {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: "Anthropic stream ended without a stop reason".to_string(),
			session: Some(Box::new(session.into_message())),
		});
	}
	if session.message.stop_reason == StopReason::Error {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: session
				.message
				.error_message
				.clone()
				.unwrap_or_else(|| "An unknown error occurred".to_string()),
			session: Some(Box::new(session.into_message())),
		});
	}
	let done_event = Event::Done {
		reason: session.message.stop_reason,
		message: session.into_message(),
	};
	forward(tx, vec![done_event]).await;
	Ok(())
}

/// Parses and forwards one batch of SSE frames. Returns `Ok(false)` when the
/// consumer dropped the stream; the producer task then stops quietly.
async fn forward_frames(
	session: &mut StreamSession,
	frames: Vec<SseFrame>,
	tx: &Sender<Event>,
) -> Result<bool, StreamFailure> {
	for frame in &frames {
		let events = session
			.process_frame(frame)
			.map_err(|message| StreamFailure {
				reason: StopReason::Error,
				message,
				session: Some(Box::new(session.message.clone())),
			})?;
		if !forward(tx, events).await {
			return Ok(false);
		}
	}
	Ok(true)
}

/// Sends events to the consumer. Returns false once the receiver is gone.
async fn forward(tx: &Sender<Event>, events: Vec<Event>) -> bool {
	for event in events {
		if tx.send(event).await.is_err() {
			return false;
		}
	}
	true
}

async fn open_response(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
) -> Result<reqwest::Response, String> {
	let key_env = "ANTHROPIC_API_KEY";
	let key = options
		.api_key
		.clone()
		.or_else(|| std::env::var(key_env).ok())
		.filter(|s| !s.is_empty())
		.ok_or_else(|| format!("{key_env} not set"))?;
	let base = if model.base_url.is_empty() {
		"https://api.anthropic.com"
	} else {
		model.base_url.trim_end_matches('/')
	};
	let response = reqwest::Client::new()
		.post(format!("{base}/v1/messages"))
		.header("accept", "application/json")
		.header("x-api-key", key)
		.header("anthropic-version", ANTHROPIC_VERSION)
		.json(&request_body(model, context, options))
		.send()
		.await
		.map_err(|e| format!("request failed: {e}"))?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		return Err(format!(
			"provider returned {status}: {}",
			anthropic_error_message(&body)
		));
	}
	Ok(response)
}

/// Extracts the message from an Anthropic error body
/// (`{"type":"error","error":{"type":"...","message":"..."}}`); falls back to
/// the raw body when it is not that shape.
fn anthropic_error_message(body: &str) -> String {
	let trimmed = body.trim();
	if trimmed.is_empty() {
		return "request failed".to_string();
	}
	serde_json::from_str::<Value>(trimmed)
		.ok()
		.and_then(|value| {
			value["error"]["message"]
				.as_str()
				.map(str::to_string)
				.or_else(|| value["error"]["type"].as_str().map(str::to_string))
		})
		.unwrap_or_else(|| trimmed.to_string())
}

/// Renders the collapsed system prompt: every system message's text joined,
/// section overrides patched by name — `getCurrentSystemMessage` +
/// `getSystemMessageText` for the transcript the TS adapter collapses
/// (`supportsMidConvoSystemMessages` is not modeled, so it always collapses).
fn system_prompt(context: &TranscriptContext) -> String {
	let mut content: Vec<String> = Vec::new();
	let mut sections: BTreeMap<String, String> = BTreeMap::new();
	for message in &context.messages {
		let Message::System(m) = message else {
			continue;
		};
		let text = match &m.content {
			SystemContent::Text(s) => s.clone(),
			SystemContent::Blocks(b) => b
				.iter()
				.map(|x| x.text.as_str())
				.collect::<Vec<_>>()
				.join("\n"),
		};
		if !text.is_empty() {
			content.push(text);
		}
		for (name, value) in m.sections.iter().flatten() {
			match value {
				Some(text) => {
					sections.insert(name.clone(), text.clone());
				}
				None => {
					sections.remove(name);
				}
			}
		}
	}
	content.extend(sections.into_values());
	content
		.into_iter()
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>()
		.join("\n\n")
}

/// Current tool set after replaying every `tools_added`/`tools_removed`
/// delta in order (port of `getCurrentTools`), preserving declaration order.
fn current_tools(context: &TranscriptContext) -> Vec<Tool> {
	let mut tools: Vec<(String, Tool)> = Vec::new();
	for message in &context.messages {
		let Message::System(m) = message else {
			continue;
		};
		for removed in m.tools_removed.iter().flatten() {
			tools.retain(|(name, _)| name != &removed.name);
		}
		for added in m.tools_added.iter().flatten() {
			match tools.iter_mut().find(|(name, _)| name == &added.name) {
				Some(slot) => slot.1 = added.clone(),
				None => tools.push((added.name.clone(), added.clone())),
			}
		}
	}
	tools.into_iter().map(|(_, tool)| tool).collect()
}

/// Tool result content in wire form — port of `convertContentBlocks`: plain
/// text collapses to one joined string, images force a block array.
fn tool_result_content(content: &[TextOrImage]) -> Value {
	let has_images = content
		.iter()
		.any(|block| matches!(block, TextOrImage::Image(_)));
	if !has_images {
		let text = content
			.iter()
			.filter_map(|block| block.text_content())
			.collect::<Vec<_>>()
			.join("\n");
		return json!(text);
	}
	let mut blocks: Vec<Value> = content
		.iter()
		.map(|block| match block {
			TextOrImage::Text(t) => json!({"type": "text", "text": t.text}),
			TextOrImage::Image(i) => image_block(i),
		})
		.collect();
	if !blocks.iter().any(|block| block["type"] == "text") {
		blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
	}
	json!(blocks)
}

fn image_block(image: &crate::types::ImageContent) -> Value {
	json!({
		"type": "image",
		"source": {"type": "base64", "media_type": image.mime_type, "data": image.data},
	})
}

/// /v1/messages request body — port of `buildParams` for the non-OAuth,
/// budget-thinking path (no `compat` overrides, no effort markers).
fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	let system = system_prompt(context);
	let tools = current_tools(context);
	let mut messages: Vec<Value> = Vec::new();

	let mut index = 0;
	while index < context.messages.len() {
		match &context.messages[index] {
			Message::System(_) => {
				// Collapsed into the top-level system prompt.
				index += 1;
			}
			Message::User(m) => {
				match &m.content {
					UserContent::Text(s) => {
						if !s.trim().is_empty() {
							messages.push(json!({"role": "user", "content": s}));
						}
					}
					UserContent::Blocks(blocks) => {
						let wire: Vec<Value> = blocks
							.iter()
							.filter_map(|block| match block {
								TextOrImage::Text(t) => (!t.text.trim().is_empty())
									.then(|| json!({"type": "text", "text": t.text})),
								TextOrImage::Image(i) => Some(image_block(i)),
							})
							.collect();
						if !wire.is_empty() {
							messages.push(json!({"role": "user", "content": wire}));
						}
					}
				}
				index += 1;
			}
			Message::Assistant(m) => {
				let mut blocks: Vec<Value> = Vec::new();
				for block in &m.content {
					match block {
						AssistantContent::Text(t) => {
							if !t.text.trim().is_empty() {
								blocks.push(json!({"type": "text", "text": t.text}));
							}
						}
						AssistantContent::Thinking(t) => {
							if t.redacted == Some(true) {
								blocks.push(json!({
									"type": "redacted_thinking",
									"data": t.thinking_signature.clone().unwrap_or_default(),
								}));
								continue;
							}
							let signature = t.thinking_signature.as_deref().unwrap_or("").trim();
							if t.thinking.trim().is_empty() && signature.is_empty() {
								continue;
							}
							if signature.is_empty() {
								// Missing signature (e.g. aborted stream): replay
								// as plain text; Anthropic rejects empty ones.
								blocks.push(json!({"type": "text", "text": t.thinking}));
							} else {
								blocks.push(json!({
									"type": "thinking",
									"thinking": t.thinking,
									"signature": t.thinking_signature,
								}));
							}
						}
						AssistantContent::ToolCall(c) => blocks.push(json!({
							"type": "tool_use",
							"id": c.id,
							"name": c.name,
							"input": c.arguments,
						})),
					}
				}
				if !blocks.is_empty() {
					messages.push(json!({"role": "assistant", "content": blocks}));
				}
				index += 1;
			}
			Message::ToolResult(_) => {
				// Consecutive tool results share one user message.
				let mut results: Vec<Value> = Vec::new();
				while let Some(Message::ToolResult(m)) = context.messages.get(index) {
					results.push(json!({
						"type": "tool_result",
						"tool_use_id": m.tool_call_id,
						"content": tool_result_content(&m.content),
						"is_error": m.is_error,
					}));
					index += 1;
				}
				messages.push(json!({"role": "user", "content": results}));
			}
		}
	}

	// Cache breakpoint on the final user block so the conversation prefix is
	// cacheable (port of the convertMessages tail).
	if let Some(last) = messages.last_mut()
		&& last["role"] == "user"
	{
		if let Some(text) = last["content"].as_str().map(str::to_string) {
			last["content"] =
				json!([{"type": "text", "text": text, "cache_control": cache_control()}]);
		} else if let Some(blocks) = last["content"].as_array_mut()
			&& let Some(last_block) = blocks.last_mut()
		{
			last_block["cache_control"] = cache_control();
		}
	}

	let mut body = json!({
		"model": model.id,
		"messages": messages,
		"max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
		"stream": true,
	});
	if !system.is_empty() {
		body["system"] =
			json!([{"type": "text", "text": system, "cache_control": cache_control()}]);
	}
	if !tools.is_empty() {
		let definitions: Vec<Value> = tools
			.iter()
			.enumerate()
			.map(|(index, tool)| {
				let mut definition = json!({
					"name": tool.name,
					"description": tool.description,
					"eager_input_streaming": true,
					"input_schema": {
						"type": "object",
						"properties": if tool.parameters["properties"].is_null() {
							json!({})
						} else {
							tool.parameters["properties"].clone()
						},
						"required": if tool.parameters["required"].is_null() {
							json!([])
						} else {
							tool.parameters["required"].clone()
						},
					},
				});
				if index + 1 == tools.len() {
					definition["cache_control"] = cache_control();
				}
				definition
			})
			.collect();
		body["tools"] = json!(definitions);
	}
	apply_thinking(model, options, &mut body);
	// Temperature is incompatible with extended thinking.
	if options.temperature.is_some() && !thinking_requested(options) {
		body["temperature"] = json!(options.temperature);
	}
	body
}

fn thinking_requested(options: &StreamOptions) -> bool {
	matches!(options.reasoning, Some(level) if level != ThinkingLevel::Off)
}

/// Thinking configuration — port of the `streamSimple` budget path:
/// `thinkingBudgetForLevel` + `adjustMaxTokensForThinking` + the final
/// `Math.min(budget, max(0, maxTokens - 1024))` clamp. Off requests disable
/// thinking explicitly on reasoning models (`thinkingDisabled` upstream).
fn apply_thinking(model: &Model, options: &StreamOptions, body: &mut Value) {
	if thinking_requested(options) {
		let budget = thinking_budget_for_level(options.reasoning.unwrap_or(ThinkingLevel::Off));
		// The request cap grows by the budget but never past the model cap.
		let max_tokens =
			(options.max_tokens.unwrap_or(model.max_tokens) + budget).min(model.max_tokens);
		let budget = budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
		body["max_tokens"] = json!(max_tokens);
		if model.reasoning {
			body["thinking"] = json!({
				"type": "enabled",
				"budget_tokens": if budget == 0 { 1024 } else { budget },
				"display": "summarized",
			});
		}
	} else if model.reasoning {
		body["thinking"] = json!({"type": "disabled"});
	}
}

/// Port of `thinkingBudgetForLevel` with the `clampReasoning` fold of
/// xhigh/max into high (`DEFAULT_THINKING_BUDGETS`).
fn thinking_budget_for_level(level: ThinkingLevel) -> u64 {
	match level {
		ThinkingLevel::Minimal => 1024,
		ThinkingLevel::Low => 2048,
		ThinkingLevel::Medium => 8192,
		ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16384,
		ThinkingLevel::Off => 0,
	}
}

/// One decoded SSE frame: the `event:` name plus the joined `data:` payload
/// and the raw lines (kept for parse-error messages, as upstream does).
struct SseFrame {
	event: Option<String>,
	data: String,
	raw: Vec<String>,
}

/// Incremental SSE frame reader — port of `iterateSseMessages`: buffers body
/// bytes, splits lines on CR, LF, or CRLF, tracks the `event:` field, joins
/// multiple `data:` lines with `\n`, and flushes a frame at each blank line.
#[derive(Default)]
struct SseDecoder {
	buffer: Vec<u8>,
	event: Option<String>,
	data: String,
	raw: Vec<String>,
}

impl SseDecoder {
	fn feed(&mut self, chunk: &[u8], frames: &mut Vec<SseFrame>) {
		self.buffer.extend_from_slice(chunk);
		while let Some(break_index) = self
			.buffer
			.iter()
			.position(|&byte| byte == b'\r' || byte == b'\n')
		{
			let line: Vec<u8> = self.buffer.drain(..break_index).collect();
			let delimiter = self.buffer.remove(0);
			if delimiter == b'\r' && self.buffer.first() == Some(&b'\n') {
				self.buffer.remove(0);
			}
			let line = String::from_utf8_lossy(&line).into_owned();
			self.process_line(&line, frames);
		}
	}

	/// Processes a final line that ended at EOF without a newline, then
	/// flushes any event still open when the server closed the stream.
	fn finish(&mut self, frames: &mut Vec<SseFrame>) {
		if !self.buffer.is_empty() {
			let bytes = std::mem::take(&mut self.buffer);
			let line = String::from_utf8_lossy(&bytes).into_owned();
			self.process_line(&line, frames);
		}
		self.flush(frames);
	}

	fn process_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
		if line.is_empty() {
			// Blank line: end of one SSE event.
			self.flush(frames);
			return;
		}
		self.raw.push(line.to_string());
		if line.starts_with(':') {
			return; // comment
		}
		let (field, value) = match line.find(':') {
			Some(index) => {
				let value = &line[index + 1..];
				(&line[..index], value.strip_prefix(' ').unwrap_or(value))
			}
			None => (line, ""),
		};
		if field == "event" {
			self.event = Some(value.to_string());
		} else if field == "data" {
			self.data.push_str(value);
			self.data.push('\n');
		}
	}

	/// Emits the open frame, if any. `raw` only resets on an actual emit,
	/// matching `flushSseEvent`.
	fn flush(&mut self, frames: &mut Vec<SseFrame>) {
		if self.event.is_none() && self.data.is_empty() {
			return;
		}
		let mut data = std::mem::take(&mut self.data);
		data.pop(); // join newline between `data:` lines
		frames.push(SseFrame {
			event: self.event.take(),
			data,
			raw: std::mem::take(&mut self.raw),
		});
	}
}

/// Stream state of one open Anthropic content block, keyed by the event's
/// block `index` and kept off the content blocks so it never reaches partials.
struct OpenBlock {
	content_index: usize,
	arguments_text: String,
}

/// The normalized content kind an open block maps to; deltas for a block
/// whose slot does not match the delta type are ignored, as upstream does.
#[derive(Clone, Copy, PartialEq)]
enum OpenKind {
	Text,
	Thinking,
	ToolCall,
}

/// Accumulates the assistant response and turns decoded SSE frames into
/// protocol events — port of the event loop in
/// `pi/packages/ai/src/api/anthropic-messages.ts`.
struct StreamSession {
	model: Model,
	message: AssistantMessage,
	open_blocks: HashMap<i64, OpenBlock>,
	saw_message_start: bool,
	saw_message_stop: bool,
}

impl StreamSession {
	fn new(model: &Model) -> Self {
		Self {
			model: model.clone(),
			message: AssistantMessage {
				content: Vec::new(),
				api: model.api.as_str().to_string(),
				provider: model.provider.clone(),
				model: model.id.clone(),
				response_model: None,
				response_id: None,
				thinking_level: None,
				usage: Usage::default(),
				stop_reason: StopReason::Pending,
				error_message: None,
				raw_stop_reason: None,
				end_turn: None,
				timestamp: now_ms(),
				duration_ms: None,
			},
			open_blocks: HashMap::new(),
			saw_message_start: false,
			saw_message_stop: false,
		}
	}

	fn partial(&self) -> AssistantMessage {
		self.message.clone()
	}

	fn into_message(self) -> AssistantMessage {
		self.message
	}

	fn process_frame(&mut self, frame: &SseFrame) -> Result<Vec<Event>, String> {
		let name = frame.event.as_deref().unwrap_or("");
		if name == "error" {
			// Upstream throws with the raw SSE payload as the message.
			return Err(frame.data.clone());
		}
		if !MESSAGE_EVENTS.contains(&name) {
			return Ok(Vec::new());
		}
		let event: Value = parse_json_with_repair(&frame.data).map_err(|error| {
			format!(
				"Could not parse Anthropic SSE event {}: {error}; data={}; raw={}",
				frame.event.as_deref().unwrap_or(""),
				frame.data,
				frame.raw.join("\n")
			)
		})?;
		match event["type"].as_str() {
			Some("message_start") => self.process_message_start(&event),
			Some("content_block_start") => self.process_block_start(&event),
			Some("content_block_delta") => self.process_block_delta(&event),
			Some("content_block_stop") => self.process_block_stop(&event),
			Some("message_delta") => self.process_message_delta(&event),
			Some("message_stop") => {
				self.saw_message_stop = true;
				Ok(Vec::new())
			}
			_ => Ok(Vec::new()),
		}
	}

	fn process_message_start(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		self.saw_message_start = true;
		let message = &event["message"];
		if let Some(id) = message["id"].as_str().filter(|id| !id.is_empty()) {
			self.message.response_id = Some(id.to_string());
		}
		if let Some(response_model) = message["model"]
			.as_str()
			.filter(|name| !name.is_empty() && *name != self.model.id)
		{
			self.message.response_model = Some(response_model.to_string());
		}
		// Initial usage from message_start so input token counts survive an
		// early abort; Anthropic reports cache tokens separately from input.
		let usage = &message["usage"];
		self.message.usage.input = usage["input_tokens"].as_u64().unwrap_or(0);
		self.message.usage.output = usage["output_tokens"].as_u64().unwrap_or(0);
		self.message.usage.cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
		self.message.usage.cache_write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
		self.message.usage.cache_write_1h = Some(
			usage["cache_creation"]["ephemeral_1h_input_tokens"]
				.as_u64()
				.unwrap_or(0),
		);
		self.recompute_total_tokens();
		Ok(Vec::new())
	}

	fn process_block_start(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let index = event["index"].as_i64().unwrap_or(0);
		let block = &event["content_block"];
		let mut events = Vec::new();
		match block["type"].as_str() {
			Some("fallback") => {
				if !self.message.content.is_empty() {
					return Err(
						"Anthropic performed an unsupported mid-output model fallback".to_string(),
					);
				}
			}
			Some("text") => {
				let text = block["text"].as_str().unwrap_or("").to_string();
				self.message
					.content
					.push(AssistantContent::Text(TextContent {
						text,
						text_signature: None,
					}));
				self.open_blocks.insert(
					index,
					OpenBlock {
						content_index: self.message.content.len() - 1,
						arguments_text: String::new(),
					},
				);
				events.push(Event::TextStart {
					content_index: self.message.content.len() - 1,
					partial: self.message.clone(),
				});
			}
			Some("thinking") => {
				self.message
					.content
					.push(AssistantContent::Thinking(ThinkingContent {
						thinking: block["thinking"].as_str().unwrap_or("").to_string(),
						thinking_signature: Some(
							block["signature"].as_str().unwrap_or("").to_string(),
						),
						redacted: None,
					}));
				self.open_blocks.insert(
					index,
					OpenBlock {
						content_index: self.message.content.len() - 1,
						arguments_text: String::new(),
					},
				);
				events.push(Event::ThinkingStart {
					content_index: self.message.content.len() - 1,
					partial: self.message.clone(),
				});
			}
			Some("redacted_thinking") => {
				self.message
					.content
					.push(AssistantContent::Thinking(ThinkingContent {
						thinking: "[Reasoning redacted]".to_string(),
						thinking_signature: Some(block["data"].as_str().unwrap_or("").to_string()),
						redacted: Some(true),
					}));
				self.open_blocks.insert(
					index,
					OpenBlock {
						content_index: self.message.content.len() - 1,
						arguments_text: String::new(),
					},
				);
				events.push(Event::ThinkingStart {
					content_index: self.message.content.len() - 1,
					partial: self.message.clone(),
				});
			}
			Some("tool_use") => {
				self.message
					.content
					.push(AssistantContent::ToolCall(ToolCall {
						id: block["id"].as_str().unwrap_or("").to_string(),
						name: block["name"].as_str().unwrap_or("").to_string(),
						arguments: if block["input"].is_null() {
							json!({})
						} else {
							block["input"].clone()
						},
						thought_signature: None,
						namespace: None,
						arguments_error: None,
					}));
				self.open_blocks.insert(
					index,
					OpenBlock {
						content_index: self.message.content.len() - 1,
						arguments_text: String::new(),
					},
				);
				events.push(Event::ToolCallStart {
					content_index: self.message.content.len() - 1,
					partial: self.message.clone(),
				});
			}
			// Server-side tool blocks and other unknown types carry no
			// normalized content; their deltas find no open block.
			_ => {}
		}
		Ok(events)
	}

	/// The normalized kind of the content slot an open block points at.
	fn slot_kind(content: Option<&AssistantContent>) -> Option<OpenKind> {
		content.map(|block| match block {
			AssistantContent::Text(_) => OpenKind::Text,
			AssistantContent::Thinking(_) => OpenKind::Thinking,
			AssistantContent::ToolCall(_) => OpenKind::ToolCall,
		})
	}

	fn process_block_delta(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let index = event["index"].as_i64().unwrap_or(0);
		let delta = &event["delta"];
		let Some(open) = self.open_blocks.get(&index) else {
			return Ok(Vec::new());
		};
		let content_index = open.content_index;
		let kind = Self::slot_kind(self.message.content.get(content_index));
		let mut events = Vec::new();
		match (delta["type"].as_str(), kind) {
			(Some("text_delta"), Some(OpenKind::Text)) => {
				let text = delta["text"].as_str().unwrap_or("");
				if let Some(AssistantContent::Text(block)) =
					self.message.content.get_mut(content_index)
				{
					block.text.push_str(text);
				}
				events.push(Event::TextDelta {
					content_index,
					delta: text.to_string(),
					partial: self.message.clone(),
				});
			}
			(Some("thinking_delta"), Some(OpenKind::Thinking)) => {
				let text = delta["thinking"].as_str().unwrap_or("");
				if let Some(AssistantContent::Thinking(block)) =
					self.message.content.get_mut(content_index)
				{
					block.thinking.push_str(text);
				}
				events.push(Event::ThinkingDelta {
					content_index,
					delta: text.to_string(),
					partial: self.message.clone(),
				});
			}
			(Some("input_json_delta"), Some(OpenKind::ToolCall)) => {
				let piece = delta["partial_json"].as_str().unwrap_or("");
				if let Some(open) = self.open_blocks.get_mut(&index) {
					open.arguments_text.push_str(piece);
					let arguments = lenient_partial_json(&open.arguments_text);
					if let Some(AssistantContent::ToolCall(block)) =
						self.message.content.get_mut(content_index)
					{
						block.arguments = arguments;
					}
				}
				events.push(Event::ToolCallDelta {
					content_index,
					delta: piece.to_string(),
					partial: self.message.clone(),
				});
			}
			(Some("signature_delta"), Some(OpenKind::Thinking)) => {
				let signature = delta["signature"].as_str().unwrap_or("");
				if let Some(AssistantContent::Thinking(block)) =
					self.message.content.get_mut(content_index)
				{
					let current = block.thinking_signature.get_or_insert_with(String::new);
					current.push_str(signature);
				}
			}
			_ => {}
		}
		Ok(events)
	}

	fn process_block_stop(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		let index = event["index"].as_i64().unwrap_or(0);
		let Some(open) = self.open_blocks.remove(&index) else {
			return Ok(Vec::new());
		};
		let content_index = open.content_index;
		let mut events = Vec::new();
		match self.message.content.get(content_index) {
			Some(AssistantContent::Text(block)) => {
				events.push(Event::TextEnd {
					content_index,
					content: block.text.clone(),
					partial: self.message.clone(),
				});
			}
			Some(AssistantContent::Thinking(thinking)) => {
				events.push(Event::ThinkingEnd {
					content_index,
					content: thinking.thinking.clone(),
					partial: self.message.clone(),
				});
			}
			Some(AssistantContent::ToolCall(call)) => {
				let (arguments, arguments_error) = parse_tool_call_arguments(&open.arguments_text);
				let mut call = call.clone();
				call.arguments = arguments;
				call.arguments_error = arguments_error;
				self.message.content[content_index] = AssistantContent::ToolCall(call.clone());
				events.push(Event::ToolCallEnd {
					content_index,
					tool_call: call,
					partial: self.message.clone(),
				});
			}
			None => {}
		}
		Ok(events)
	}

	fn process_message_delta(&mut self, event: &Value) -> Result<Vec<Event>, String> {
		if let Some(stop_reason) = event["delta"]["stop_reason"]
			.as_str()
			.filter(|reason| !reason.is_empty())
		{
			self.message.raw_stop_reason = Some(stop_reason.to_string());
			let (reason, error) = map_stop_reason(stop_reason, &event["delta"]["stop_details"])?;
			self.message.stop_reason = reason;
			if let Some(error) = error {
				self.message.error_message = Some(error);
			}
		}
		// Only update usage fields that are present, so message_start input
		// counts survive proxies that omit them in message_delta.
		let usage = &event["usage"];
		if !usage.is_null() {
			if let Some(value) = usage["input_tokens"].as_u64() {
				self.message.usage.input = value;
			}
			if let Some(value) = usage["output_tokens"].as_u64() {
				self.message.usage.output = value;
			}
			if let Some(value) = usage["cache_read_input_tokens"].as_u64() {
				self.message.usage.cache_read = value;
			}
			if let Some(value) = usage["cache_creation_input_tokens"].as_u64() {
				self.message.usage.cache_write = value;
			}
			if let Some(value) = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
				self.message.usage.cache_write_1h = Some(value);
			}
			// Anthropic reports reasoning tokens as a subset of output tokens.
			if let Some(value) = usage["output_tokens_details"]["thinking_tokens"].as_u64() {
				self.message.usage.reasoning = Some(value);
			}
		}
		self.recompute_total_tokens();
		Ok(Vec::new())
	}

	/// Anthropic does not provide total_tokens; compute from components.
	fn recompute_total_tokens(&mut self) {
		self.message.usage.total_tokens = self.message.usage.input
			+ self.message.usage.output
			+ self.message.usage.cache_read
			+ self.message.usage.cache_write;
	}
}

/// Port of `mapStopReason` in
/// `pi/packages/ai/src/api/anthropic-messages.ts`. Unknown stop reasons fail
/// the stream like the upstream throw; refusal and safety trips surface as an
/// error stop reason.
fn map_stop_reason(
	reason: &str,
	stop_details: &Value,
) -> Result<(StopReason, Option<String>), String> {
	let reason = match reason {
		"end_turn" => return Ok((StopReason::Stop, None)),
		"max_tokens" => return Ok((StopReason::Length, None)),
		"tool_use" => return Ok((StopReason::ToolUse, None)),
		"refusal" => {
			let explanation = stop_details["explanation"]
				.as_str()
				.unwrap_or("The model refused to complete the request");
			return Ok((StopReason::Error, Some(explanation.to_string())));
		}
		// Stop is good enough -> resubmit; no stop sequences are supplied.
		"pause_turn" | "stop_sequence" => return Ok((StopReason::Stop, None)),
		// Content flagged by safety filters (not yet in SDK types).
		"sensitive" => {
			return Ok((
				StopReason::Error,
				Some("Provider stopped with: sensitive".to_string()),
			));
		}
		other => other,
	};
	Err(format!("Unhandled stop reason: {reason}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{ToolResultMessage, UserMessage};

	fn model() -> Model {
		let mut model = Model::new(
			"claude-test",
			"claude-test",
			"anthropic-messages",
			"anthropic",
			"",
		);
		model.max_tokens = 4096;
		model
	}

	fn context() -> TranscriptContext {
		TranscriptContext {
			messages: vec![
				Message::System(crate::types::SystemMessage::prompt("be brief")),
				Message::user("hi"),
			],
		}
	}

	fn options() -> StreamOptions {
		StreamOptions::default()
	}

	#[test]
	fn request_body_matches_wire_shape() {
		let model = model();
		let mut system = crate::types::SystemMessage::prompt("be brief");
		system.tools_added = Some(vec![Tool::new(
			"ls",
			"list",
			json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
		)]);
		let ctx = TranscriptContext {
			messages: vec![
				Message::System(system),
				Message::user("list"),
				Message::Assistant(AssistantMessage {
					content: vec![
						AssistantContent::Text(TextContent {
							text: "checking".into(),
							text_signature: None,
						}),
						AssistantContent::ToolCall(ToolCall {
							id: "call_1".into(),
							name: "ls".into(),
							arguments: json!({"path": "."}),
							thought_signature: None,
							namespace: None,
							arguments_error: None,
						}),
					],
					..AssistantMessage::error(&model, StopReason::Pending, "")
				}),
				Message::ToolResult(ToolResultMessage::text_result(
					"call_1", "ls", "file", false,
				)),
			],
		};
		let body = request_body(&model, &ctx, &options());
		assert_eq!(body["model"], "claude-test");
		assert_eq!(body["stream"], true);
		assert_eq!(body["max_tokens"], 4096);
		assert_eq!(body["system"][0]["type"], "text");
		assert_eq!(body["system"][0]["text"], "be brief");
		assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
		let messages = body["messages"].as_array().unwrap();
		assert_eq!(messages.len(), 3, "system message stays top-level");
		assert_eq!(messages[0]["role"], "user");
		assert_eq!(messages[0]["content"], "list");
		assert_eq!(messages[1]["role"], "assistant");
		assert_eq!(messages[1]["content"][0]["type"], "text");
		assert_eq!(messages[1]["content"][1]["type"], "tool_use");
		assert_eq!(messages[1]["content"][1]["input"]["path"], ".");
		assert_eq!(messages[2]["role"], "user");
		assert_eq!(messages[2]["content"][0]["type"], "tool_result");
		assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
		assert_eq!(
			messages[2]["content"][0]["cache_control"]["type"], "ephemeral",
			"last user block carries the cache breakpoint"
		);
		assert_eq!(body["tools"][0]["name"], "ls");
		assert_eq!(body["tools"][0]["input_schema"]["required"][0], "path");
		assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
		assert!(body.get("thinking").is_none());
	}

	#[test]
	fn thinking_maps_reasoning_level_to_budget() {
		let mut model = model();
		model.reasoning = true;
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::Medium),
			..Default::default()
		};
		let body = request_body(&model, &context(), &options);
		assert_eq!(body["thinking"]["type"], "enabled");
		// budget 8192 clamped so 1024 answer tokens stay under the 4096 cap.
		assert_eq!(body["thinking"]["budget_tokens"], 4096 - 1024);
		assert_eq!(body["max_tokens"], 4096);
		assert!(body.get("temperature").is_none());
	}

	#[test]
	fn thinking_off_disables_explicitly_on_reasoning_models() {
		let mut model = model();
		model.reasoning = true;
		let body = request_body(&model, &context(), &options());
		assert_eq!(body["thinking"], json!({"type": "disabled"}));

		let plain = Model::new("plain", "plain", "anthropic-messages", "anthropic", "");
		let body = request_body(&plain, &context(), &options());
		assert!(body.get("thinking").is_none());
	}

	#[test]
	fn xhigh_clamps_to_high_budget() {
		assert_eq!(
			thinking_budget_for_level(ThinkingLevel::Xhigh),
			thinking_budget_for_level(ThinkingLevel::High)
		);
		assert_eq!(thinking_budget_for_level(ThinkingLevel::Minimal), 1024);
		assert_eq!(thinking_budget_for_level(ThinkingLevel::Max), 16384);
	}

	#[test]
	fn stop_reason_mapping_matches_upstream() {
		for (raw, expected) in [
			("end_turn", StopReason::Stop),
			("max_tokens", StopReason::Length),
			("tool_use", StopReason::ToolUse),
			("pause_turn", StopReason::Stop),
			("stop_sequence", StopReason::Stop),
		] {
			let (reason, error) = map_stop_reason(raw, &Value::Null).unwrap();
			assert_eq!(reason, expected, "{raw}");
			assert!(error.is_none());
		}
		let (reason, error) = map_stop_reason("refusal", &json!({"explanation": "nope"})).unwrap();
		assert_eq!(reason, StopReason::Error);
		assert_eq!(error.as_deref(), Some("nope"));
		let (reason, error) = map_stop_reason("refusal", &Value::Null).unwrap();
		assert_eq!(reason, StopReason::Error);
		assert_eq!(
			error.as_deref(),
			Some("The model refused to complete the request")
		);
		assert!(map_stop_reason("martian", &Value::Null).is_err());
	}

	#[test]
	fn sse_decoder_tracks_event_and_multi_line_data() {
		let mut decoder = SseDecoder::default();
		let mut frames = Vec::new();
		decoder.feed(b"event: content_block_delta\r\ndata: {\"a\":", &mut frames);
		decoder.feed(b"1}\n: keep-alive comment\n\ndata: ignored\n", &mut frames);
		assert_eq!(frames.len(), 1);
		assert_eq!(frames[0].event.as_deref(), Some("content_block_delta"));
		assert_eq!(frames[0].data, r#"{"a":1}"#);
		assert!(
			frames[0]
				.raw
				.iter()
				.any(|line| line == ": keep-alive comment")
		);
		decoder.finish(&mut frames);
		assert_eq!(
			frames.len(),
			2,
			"data-only frame without a blank line still flushes at EOF"
		);
		assert_eq!(frames[1].event, None);
		assert_eq!(frames[1].data, "ignored");
	}

	#[test]
	fn user_message_with_images_becomes_block_array() {
		let ctx = TranscriptContext {
			messages: vec![Message::User(UserMessage {
				content: crate::types::UserContent::Blocks(vec![
					TextOrImage::text("look"),
					TextOrImage::Image(crate::types::ImageContent {
						data: "abc".into(),
						mime_type: "image/png".into(),
					}),
				]),
				timestamp: 0,
			})],
		};
		let body = request_body(&model(), &ctx, &options());
		assert_eq!(body["messages"][0]["content"][0]["type"], "text");
		assert_eq!(
			body["messages"][0]["content"][1]["source"]["media_type"],
			"image/png"
		);
	}
}
