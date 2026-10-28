//! OpenAI-compatible Chat Completions adapter — port of the streaming
//! `stream()` in `pi/packages/ai/src/api/openai-completions.ts`.
//!
//! The response body is consumed incrementally: SSE `data:` frames are parsed
//! as they arrive and each delta immediately emits protocol events, so agent
//! consumers see text/tool-call updates while the response is still in
//! flight. Tool-call arguments accumulate per call in stream state (never on
//! the partial message) and are parsed once when the call finishes
//! (`pi/packages/ai/src/utils/streaming-json.ts` and
//! `pi/packages/ai/src/utils/json-parse.ts` semantics).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::json_repair::{lenient_partial_json, parse_tool_call_arguments};
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, Message, StopReason, SystemContent, TextContent,
	TextOrImage, ThinkingContent, ToolCall, Usage, UserContent, now_ms,
};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Reasoning-only delta fields some OpenAI-compatible endpoints use instead of
/// `content` (llama.cpp, OpenRouter, ...).
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

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
	let mut done = false;
	while !done {
		let chunk = match response.chunk().await {
			Ok(Some(chunk)) => chunk,
			Ok(None) => {
				let mut payloads = Vec::new();
				decoder.finish(&mut payloads);
				if !forward_payloads(&mut session, payloads, tx).await? {
					return Ok(());
				}
				break;
			}
			Err(error) => {
				return Err(StreamFailure {
					reason: StopReason::Error,
					message: format!("stream read failed: {error}"),
					session: Some(Box::new(session.into_message())),
				});
			}
		};
		let mut payloads = Vec::new();
		decoder.feed(&chunk, &mut payloads);
		if !forward_payloads(&mut session, payloads, tx).await? {
			return Ok(());
		}
		done = decoder.is_done();
	}

	// The synthesized end events (text/thinking/toolcall) must reach the
	// consumer before Done; finish() closes any blocks still open.
	let finished_events = session.finish();
	if !forward(tx, finished_events).await {
		return Ok(());
	}
	if session.message.stop_reason == StopReason::Error {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: session
				.message
				.error_message
				.clone()
				.unwrap_or_else(|| "provider returned an error stop reason".to_string()),
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

/// Parses and forwards one batch of SSE payloads. Returns `Ok(false)` when the
/// consumer dropped the stream; the producer task then stops quietly.
async fn forward_payloads(
	session: &mut StreamSession,
	payloads: Vec<String>,
	tx: &Sender<Event>,
) -> Result<bool, StreamFailure> {
	for payload in &payloads {
		let events = session
			.process_payload(payload)
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
	let key_env = if model.provider == "mimo" {
		"MIMO_API_KEY"
	} else {
		"OPENAI_API_KEY"
	};
	let key = options
		.api_key
		.clone()
		.or_else(|| std::env::var(key_env).ok())
		.filter(|s| !s.is_empty())
		.ok_or_else(|| format!("{key_env} not set"))?;
	let base = if model.base_url.is_empty() {
		"https://api.openai.com/v1"
	} else {
		model.base_url.trim_end_matches('/')
	};
	let response = reqwest::Client::new()
		.post(format!("{base}/chat/completions"))
		.bearer_auth(key)
		.json(&request_body(model, context, options))
		.send()
		.await
		.map_err(|e| format!("request failed: {e}"))?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		let message = serde_json::from_str::<Value>(&body)
			.ok()
			.and_then(|value| value["error"]["message"].as_str().map(str::to_string))
			.unwrap_or_else(|| {
				if body.trim().is_empty() {
					"request failed".to_string()
				} else {
					body.trim().to_string()
				}
			});
		return Err(format!("provider returned {status}: {message}"));
	}
	Ok(response)
}

fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	let mut messages = Vec::new();
	let mut tools = Vec::new();
	for message in &context.messages {
		match message {
			Message::System(m) => {
				let text = match &m.content {
					SystemContent::Text(s) => s.clone(),
					SystemContent::Blocks(b) => b
						.iter()
						.map(|x| x.text.as_str())
						.collect::<Vec<_>>()
						.join("\n"),
				};
				if !text.is_empty() {
					messages.push(json!({"role":"system","content":text}));
				}
				if let Some(declarations) = &m.tools_added {
					tools.extend(declarations.iter().map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}})));
				}
			}
			Message::User(m) => {
				let content = match &m.content {
                    UserContent::Text(s) => json!(s),
                    UserContent::Blocks(blocks) => json!(blocks.iter().map(|b| match b {
                        TextOrImage::Text(t) => json!({"type":"text","text":t.text}),
                        TextOrImage::Image(i) => json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",i.mime_type,i.data)}}),
                    }).collect::<Vec<_>>()),
                };
				messages.push(json!({"role":"user","content":content}));
			}
			Message::Assistant(m) => {
				let calls: Vec<_> = m.tool_calls().iter().map(|t| json!({"id":t.id,"type":"function","function":{"name":t.name,"arguments":t.arguments.to_string()}})).collect();
				let mut value = json!({"role":"assistant","content":if m.text().is_empty() { Value::Null } else { json!(m.text()) }});
				if !calls.is_empty() {
					value["tool_calls"] = json!(calls);
				}
				messages.push(value);
			}
			Message::ToolResult(m) => messages
				.push(json!({"role":"tool","tool_call_id":m.tool_call_id,"content":m.text()})),
		}
	}
	let mut body = json!({
		"model": model.id,
		"messages": messages,
		"stream": true,
		"stream_options": {"include_usage": true},
	});
	if !tools.is_empty() {
		body["tools"] = json!(tools);
	}
	if let Some(t) = options.temperature {
		body["temperature"] = json!(t);
	}
	if let Some(t) = options.max_tokens {
		body["max_completion_tokens"] = json!(t);
	}
	body
}

/// Incremental SSE frame reader: buffers body bytes, extracts complete lines
/// and collects the `data:` payload of each event (multiple `data:` lines join
/// with `\n`), stopping at the `[DONE]` sentinel. This mirrors the SSE
/// decoding the OpenAI SDK performs around the TS adapter's chunk loop.
#[derive(Default)]
struct SseDecoder {
	buffer: Vec<u8>,
	data: String,
	done: bool,
}

impl SseDecoder {
	fn feed(&mut self, chunk: &[u8], payloads: &mut Vec<String>) {
		self.buffer.extend_from_slice(chunk);
		while !self.done {
			let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') else {
				break;
			};
			let mut line: Vec<u8> = self.buffer.drain(..newline).collect();
			self.buffer.remove(0);
			if line.last() == Some(&b'\r') {
				line.pop();
			}
			self.process_line(&String::from_utf8_lossy(&line), payloads);
		}
	}

	/// Flushes a final line that ended at EOF without a newline, plus any
	/// event still open when the server closed the stream.
	fn finish(&mut self, payloads: &mut Vec<String>) {
		if !self.buffer.is_empty() {
			let bytes = std::mem::take(&mut self.buffer);
			let line = String::from_utf8_lossy(&bytes).into_owned();
			self.process_line(line.trim_end_matches('\r'), payloads);
		}
		if !self.data.is_empty() {
			payloads.push(self.take_data());
		}
	}

	fn is_done(&self) -> bool {
		self.done
	}

	fn process_line(&mut self, line: &str, payloads: &mut Vec<String>) {
		if line.is_empty() {
			// Blank line: end of one SSE event.
			if !self.data.is_empty() {
				payloads.push(self.take_data());
			}
			return;
		}
		if let Some(rest) = line.strip_prefix("data:") {
			let value = rest.strip_prefix(' ').unwrap_or(rest);
			if value.trim() == "[DONE]" {
				if !self.data.is_empty() {
					payloads.push(self.take_data());
				}
				self.done = true;
			} else {
				self.data.push_str(value);
				self.data.push('\n');
			}
		}
		// `:` comments and other SSE fields carry no data for this API.
	}

	fn take_data(&mut self) -> String {
		let mut data = std::mem::take(&mut self.data);
		data.pop(); // join newline between `data:` lines
		data
	}
}

/// Accumulated stream state of one tool call, keyed by the chunk's `index`
/// (or id). The raw argument text lives here, never on the partial message.
struct ToolAccumulator {
	content_index: usize,
	id: String,
	name: String,
	arguments_text: String,
	started: bool,
}

/// Accumulates the assistant response and turns parsed SSE payloads into
/// protocol events — port of the chunk loop in
/// `pi/packages/ai/src/api/openai-completions.ts`.
struct StreamSession {
	model: Model,
	message: AssistantMessage,
	text_index: Option<usize>,
	thinking_index: Option<usize>,
	tools: Vec<ToolAccumulator>,
	tool_slot_by_stream_index: HashMap<i64, usize>,
	tool_slot_by_id: HashMap<String, usize>,
	has_finish_reason: bool,
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
			text_index: None,
			thinking_index: None,
			tools: Vec::new(),
			tool_slot_by_stream_index: HashMap::new(),
			tool_slot_by_id: HashMap::new(),
			has_finish_reason: false,
		}
	}

	fn partial(&self) -> AssistantMessage {
		self.message.clone()
	}

	fn into_message(self) -> AssistantMessage {
		self.message
	}

	fn process_payload(&mut self, payload: &str) -> Result<Vec<Event>, String> {
		let payload = payload.trim();
		if payload.is_empty() {
			return Ok(Vec::new());
		}
		let chunk: Value = serde_json::from_str(payload)
			.map_err(|error| format!("malformed SSE payload: {error}"))?;
		self.process_chunk(&chunk)
	}

	fn process_chunk(&mut self, chunk: &Value) -> Result<Vec<Event>, String> {
		let mut events = Vec::new();
		// Every chunk of one completion carries the same id/model; first wins.
		if self.message.response_id.is_none()
			&& let Some(id) = chunk["id"].as_str().filter(|id| !id.is_empty())
		{
			self.message.response_id = Some(id.to_string());
		}
		if self.message.response_model.is_none()
			&& let Some(name) = chunk["model"]
				.as_str()
				.filter(|name| !name.is_empty() && *name != self.model.id)
		{
			self.message.response_model = Some(name.to_string());
		}
		if !chunk["usage"].is_null() {
			self.message.usage = parse_usage(&chunk["usage"]);
		}

		let Some(choice) = chunk["choices"].as_array().and_then(|c| c.first()) else {
			return Ok(events);
		};
		// Some providers (e.g. Moonshot) report usage on the choice instead.
		if chunk["usage"].is_null() && choice["usage"].is_object() {
			self.message.usage = parse_usage(&choice["usage"]);
		}
		if let Some(finish) = choice["finish_reason"].as_str() {
			self.message.raw_stop_reason = Some(finish.to_string());
			let (reason, error) = map_finish_reason(finish);
			self.message.stop_reason = reason;
			if let Some(error) = error {
				self.message.error_message = Some(error);
			}
			self.has_finish_reason = true;
		}

		let delta = &choice["delta"];
		if delta.is_null() {
			return Ok(events);
		}
		// Reasoning before content: some proxies aggregate reasoning and answer
		// into one delta, and the thinking block must precede the text block.
		for field in REASONING_FIELDS {
			if let Some(text) = delta[field].as_str().filter(|t| !t.is_empty()) {
				let index = self.ensure_thinking(field, &mut events);
				if let Some(AssistantContent::Thinking(block)) = self.message.content.get_mut(index)
				{
					block.thinking.push_str(text);
				}
				events.push(Event::ThinkingDelta {
					content_index: index,
					delta: text.to_string(),
					partial: self.message.clone(),
				});
				break;
			}
		}
		if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
			let index = self.ensure_text(&mut events);
			if let Some(AssistantContent::Text(block)) = self.message.content.get_mut(index) {
				block.text.push_str(text);
			}
			events.push(Event::TextDelta {
				content_index: index,
				delta: text.to_string(),
				partial: self.message.clone(),
			});
		}
		if let Some(calls) = delta["tool_calls"].as_array() {
			for call in calls {
				self.process_tool_delta(call, &mut events);
			}
		}
		Ok(events)
	}

	fn ensure_text(&mut self, events: &mut Vec<Event>) -> usize {
		if let Some(index) = self.text_index {
			return index;
		}
		let index = self.message.content.len();
		self.message
			.content
			.push(AssistantContent::Text(TextContent {
				text: String::new(),
				text_signature: None,
			}));
		self.text_index = Some(index);
		events.push(Event::TextStart {
			content_index: index,
			partial: self.message.clone(),
		});
		index
	}

	fn ensure_thinking(&mut self, signature: &str, events: &mut Vec<Event>) -> usize {
		if let Some(index) = self.thinking_index {
			return index;
		}
		let index = self.message.content.len();
		self.message
			.content
			.push(AssistantContent::Thinking(ThinkingContent {
				thinking: String::new(),
				thinking_signature: Some(signature.to_string()),
				redacted: None,
			}));
		self.thinking_index = Some(index);
		events.push(Event::ThinkingStart {
			content_index: index,
			partial: self.message.clone(),
		});
		index
	}

	fn process_tool_delta(&mut self, call: &Value, events: &mut Vec<Event>) {
		let stream_index = call["index"].as_i64();
		let id = call["id"].as_str().unwrap_or("");
		let name = call["function"]["name"]
			.as_str()
			.or_else(|| call["custom"]["name"].as_str())
			.unwrap_or("");

		let mut slot = stream_index.and_then(|i| self.tool_slot_by_stream_index.get(&i).copied());
		if slot.is_none() && !id.is_empty() {
			slot = self.tool_slot_by_id.get(id).copied();
		}
		let slot = match slot {
			Some(slot) => slot,
			None => {
				let content_index = self.message.content.len();
				self.message
					.content
					.push(AssistantContent::ToolCall(ToolCall {
						id: id.to_string(),
						name: name.to_string(),
						arguments: json!({}),
						thought_signature: None,
						namespace: None,
						arguments_error: None,
					}));
				let slot = self.tools.len();
				self.tools.push(ToolAccumulator {
					content_index,
					id: id.to_string(),
					name: name.to_string(),
					arguments_text: String::new(),
					started: false,
				});
				if let Some(index) = stream_index {
					self.tool_slot_by_stream_index.insert(index, slot);
				}
				if !id.is_empty() {
					self.tool_slot_by_id.insert(id.to_string(), slot);
				}
				slot
			}
		};

		let accumulator = &mut self.tools[slot];
		if accumulator.id.is_empty() && !id.is_empty() {
			accumulator.id = id.to_string();
		}
		if accumulator.name.is_empty() && !name.is_empty() {
			accumulator.name = name.to_string();
		}
		let delta = call["function"]["arguments"].as_str().unwrap_or("");
		accumulator.arguments_text.push_str(delta);
		let arguments = lenient_partial_json(&accumulator.arguments_text);
		let (id, name) = (accumulator.id.clone(), accumulator.name.clone());
		let content_index = accumulator.content_index;
		let should_start = !accumulator.started && !id.is_empty() && !name.is_empty();
		let delta_to_emit = if should_start {
			accumulator.arguments_text.clone()
		} else {
			delta.to_string()
		};
		if should_start {
			accumulator.started = true;
		}
		let started = accumulator.started;
		if let Some(AssistantContent::ToolCall(block)) = self.message.content.get_mut(content_index)
		{
			block.id = id;
			block.name = name;
			block.arguments = arguments;
		}
		if should_start {
			events.push(Event::ToolCallStart {
				content_index,
				partial: self.message.clone(),
			});
		}
		if started {
			events.push(Event::ToolCallDelta {
				content_index,
				delta: delta_to_emit,
				partial: self.message.clone(),
			});
		}
	}

	/// Ends open blocks in content order and applies the missing-finish-reason
	/// fallback (provider compat flags are not modeled, so the lenient TS
	/// branch always applies).
	fn finish(&mut self) -> Vec<Event> {
		let mut events = Vec::new();
		let mut tool_cursor = 0;
		for index in 0..self.message.content.len() {
			match &self.message.content[index] {
				AssistantContent::Text(block) => {
					events.push(Event::TextEnd {
						content_index: index,
						content: block.text.clone(),
						partial: self.message.clone(),
					});
				}
				AssistantContent::Thinking(thinking) => {
					events.push(Event::ThinkingEnd {
						content_index: index,
						content: thinking.thinking.clone(),
						partial: self.message.clone(),
					});
				}
				AssistantContent::ToolCall(_) => {
					if !self.tools[tool_cursor].started {
						events.push(Event::ToolCallStart {
							content_index: index,
							partial: self.message.clone(),
						});
						if !self.tools[tool_cursor].arguments_text.is_empty() {
							events.push(Event::ToolCallDelta {
								content_index: index,
								delta: self.tools[tool_cursor].arguments_text.clone(),
								partial: self.message.clone(),
							});
						}
					}
					let (arguments, arguments_error) =
						parse_tool_call_arguments(&self.tools[tool_cursor].arguments_text);
					tool_cursor += 1;
					let mut call = match &self.message.content[index] {
						AssistantContent::ToolCall(call) => call.clone(),
						_ => continue,
					};
					call.arguments = arguments;
					call.arguments_error = arguments_error;
					self.message.content[index] = AssistantContent::ToolCall(call.clone());
					events.push(Event::ToolCallEnd {
						content_index: index,
						tool_call: call,
						partial: self.message.clone(),
					});
				}
			}
		}
		if !self.has_finish_reason {
			self.message.stop_reason = if self.tools.is_empty() {
				StopReason::Stop
			} else {
				StopReason::ToolUse
			};
		}
		events
	}
}

/// Port of `mapStopReason` in `pi/packages/ai/src/api/openai-completions.ts`.
/// Unknown or safety-trip finish reasons surface as an error stop reason.
fn map_finish_reason(reason: &str) -> (StopReason, Option<String>) {
	match reason {
		"stop" | "end" => (StopReason::Stop, None),
		"length" => (StopReason::Length, None),
		"tool_calls" | "function_call" => (StopReason::ToolUse, None),
		other => (
			StopReason::Error,
			Some(format!("Provider finish_reason: {other}")),
		),
	}
}

/// Port of `parseChunkUsage` in `pi/packages/ai/src/api/openai-completions.ts`:
/// cache-hit tokens move out of `input` into `cache_read`/`cache_write`
/// (OpenAI `prompt_tokens_details`, DeepSeek `prompt_cache_hit_tokens`, Kimi
/// top-level `cached_tokens`). A provider-reported total wins when present.
fn parse_usage(raw: &Value) -> Usage {
	let prompt_tokens = raw["prompt_tokens"].as_u64().unwrap_or(0);
	let details = &raw["prompt_tokens_details"];
	let cache_read = details["cached_tokens"]
		.as_u64()
		.or_else(|| raw["prompt_cache_hit_tokens"].as_u64())
		.or_else(|| raw["cached_tokens"].as_u64())
		.unwrap_or(0);
	let cache_write = details["cache_write_tokens"].as_u64().unwrap_or(0);
	let input = prompt_tokens.saturating_sub(cache_read + cache_write);
	let output = raw["completion_tokens"].as_u64().unwrap_or(0);
	Usage {
		input,
		output,
		cache_read,
		cache_write,
		cache_write_1h: None,
		reasoning: raw["completion_tokens_details"]["reasoning_tokens"].as_u64(),
		total_tokens: raw["total_tokens"]
			.as_u64()
			.unwrap_or(input + output + cache_read + cache_write),
		cost: Default::default(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{Tool, ToolResultMessage};

	fn session() -> StreamSession {
		StreamSession::new(&Model::new(
			"test-model",
			"test",
			"openai-completions",
			"openai",
			"",
		))
	}

	fn chunk_json(json: &str) -> String {
		json.to_string()
	}

	#[test]
	fn request_body_requests_streaming_usage() {
		let model = Model::new("test", "test", "openai-completions", "openai", "");
		let mut system = crate::types::SystemMessage::prompt("help");
		system.tools_added = Some(vec![Tool::new("ls", "list", json!({"type":"object"}))]);
		let ctx = TranscriptContext {
			messages: vec![
				Message::System(system),
				Message::user("list"),
				Message::Assistant(assistant_with_tool_call()),
				Message::ToolResult(ToolResultMessage::text_result(
					"call_1", "ls", "file", false,
				)),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(body["stream"], true);
		assert_eq!(body["stream_options"]["include_usage"], true);
		assert_eq!(body["tools"][0]["function"]["name"], "ls");
		assert_eq!(
			body["messages"][2]["tool_calls"][0]["function"]["arguments"],
			"{\"path\":\".\"}"
		);
		assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
	}

	fn assistant_with_tool_call() -> AssistantMessage {
		let mut message = AssistantMessage::error(
			&Model::new("test", "test", "openai-completions", "openai", ""),
			StopReason::Pending,
			"",
		);
		message.content = vec![AssistantContent::ToolCall(ToolCall {
			id: "call_1".into(),
			name: "ls".into(),
			arguments: json!({"path":"."}),
			thought_signature: None,
			namespace: None,
			arguments_error: None,
		})];
		message
	}

	#[test]
	fn text_deltas_stream_and_complete() {
		let mut session = session();
		let events = session
			.process_payload(&chunk_json(
				r#"{"id":"chatcmpl-1","model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
			))
			.unwrap();
		assert_eq!(events.len(), 2);
		assert!(matches!(
			events[0],
			Event::TextStart {
				content_index: 0,
				..
			}
		));
		let Event::TextDelta { delta, partial, .. } = &events[1] else {
			panic!("expected text delta")
		};
		assert_eq!(delta, "Hel");
		assert_eq!(partial.text(), "Hel");

		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}"#,
			))
			.unwrap();
		let events = session.finish();
		assert!(matches!(events.last(), Some(Event::TextEnd { .. })));
		assert_eq!(session.message.text(), "Hello");
		assert_eq!(session.message.stop_reason, StopReason::Stop);
		assert_eq!(session.message.raw_stop_reason.as_deref(), Some("stop"));
		assert_eq!(session.message.response_id.as_deref(), Some("chatcmpl-1"));
		assert_eq!(session.message.response_model.as_deref(), Some("gpt-test"));
		assert_eq!(session.message.usage.input, 10);
		assert_eq!(session.message.usage.output, 4);
		assert_eq!(session.message.usage.total_tokens, 14);
	}

	#[test]
	fn parallel_tool_calls_accumulate_independently() {
		let mut session = session();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"ls","arguments":""}}]}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"cat","arguments":""}}]}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\".\"}"}}]}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"src\"}"}}]}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
			))
			.unwrap();

		assert_eq!(session.message.content.len(), 2);
		let events = session.finish();
		assert_eq!(session.message.stop_reason, StopReason::ToolUse);
		let ends: Vec<&Event> = events
			.iter()
			.filter(|event| matches!(event, Event::ToolCallEnd { .. }))
			.collect();
		assert_eq!(ends.len(), 2);
		let calls = session.message.tool_calls();
		assert_eq!(calls[0].id, "call_a");
		assert_eq!(calls[0].name, "ls");
		assert_eq!(calls[0].arguments, json!({"path": "."}));
		assert_eq!(calls[0].arguments_error, None);
		assert_eq!(calls[1].id, "call_b");
		assert_eq!(calls[1].name, "cat");
		assert_eq!(calls[1].arguments, json!({"path": "src"}));
	}

	#[test]
	fn malformed_tool_arguments_surface_as_arguments_error() {
		let mut session = session();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"ls","arguments":"{"}}]}}]}"#,
			))
			.unwrap();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
			))
			.unwrap();
		session.finish();
		let calls = session.message.tool_calls();
		assert_eq!(calls[0].arguments, json!({}));
		assert!(
			calls[0]
				.arguments_error
				.as_deref()
				.unwrap()
				.contains("Received arguments:")
		);
	}

	#[test]
	fn unknown_finish_reason_maps_to_error() {
		let mut session = session();
		session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#,
			))
			.unwrap();
		session.finish();
		assert_eq!(session.message.stop_reason, StopReason::Error);
		assert_eq!(
			session.message.error_message.as_deref(),
			Some("Provider finish_reason: content_filter")
		);
	}

	#[test]
	fn missing_finish_reason_falls_back_by_content() {
		let mut text_session = session();
		text_session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#,
			))
			.unwrap();
		text_session.finish();
		assert_eq!(text_session.message.stop_reason, StopReason::Stop);

		let mut tool_session = session();
		tool_session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"ls","arguments":"{}"}}]}}]}"#,
			))
			.unwrap();
		tool_session.finish();
		assert_eq!(tool_session.message.stop_reason, StopReason::ToolUse);
	}

	#[test]
	fn reasoning_delta_precedes_text_delta() {
		let mut session = session();
		let events = session
			.process_payload(&chunk_json(
				r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think","content":"answer"}}]}"#,
			))
			.unwrap();
		assert_eq!(events.len(), 4);
		assert!(matches!(
			events[0],
			Event::ThinkingStart {
				content_index: 0,
				..
			}
		));
		assert!(matches!(
			events[1],
			Event::ThinkingDelta {
				content_index: 0,
				..
			}
		));
		assert!(matches!(
			events[2],
			Event::TextStart {
				content_index: 1,
				..
			}
		));
		assert!(matches!(
			events[3],
			Event::TextDelta {
				content_index: 1,
				..
			}
		));
		session.finish();
		assert_eq!(session.message.thinking(), "think");
		assert_eq!(session.message.text(), "answer");
	}

	#[test]
	fn sse_decoder_splits_frames_across_chunks() {
		let mut decoder = SseDecoder::default();
		let mut payloads = Vec::new();
		decoder.feed(b"data: {\"a\":", &mut payloads);
		decoder.feed(b"1}\n\ndata: [DONE]\n\n", &mut payloads);
		assert_eq!(payloads, vec![r#"{"a":1}"#.to_string()]);
		assert!(decoder.is_done());
	}

	#[test]
	fn lenient_partial_json_previews() {
		assert_eq!(lenient_partial_json(r#"{"path":"."}"#), json!({"path":"."}));
		assert_eq!(lenient_partial_json(r#"{"path":"."#), json!({"path":"."}));
		assert_eq!(lenient_partial_json(r#"{"pa"#), json!({}));
		assert_eq!(lenient_partial_json(r#"{"a":1,"#), json!({"a":1}));
		assert_eq!(lenient_partial_json(r#"{"n":12"#), json!({"n":12}));
		assert_eq!(lenient_partial_json(""), json!({}));
	}
}
