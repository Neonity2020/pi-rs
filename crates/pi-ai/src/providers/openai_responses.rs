//! OpenAI Responses API adapter — port of the streaming `stream()` in
//! `pi/packages/ai/src/api/openai-responses.ts` plus the message conversion
//! and stream processing of `openai-responses-shared.ts`.
//!
//! First slice: the default-compat surface (leading system message as
//! developer/system, function tools without strict mode, reasoning effort +
//! `include: ["reasoning.encrypted_content"]`, prompt-cache key, and the
//! text/thinking/function-call event mapping keyed by `output_index`).
//! Compat-gated upstream paths stay deferred with their flags defaulting
//! off: grammar/custom tools, `additional_tools`/`tool_search` items,
//! `configuration_update` items, ChatGPT sign-in special casing,
//! copilot/xai headers, service-tier pricing, long cache retention, and
//! OpenRouter session affinity. `sanitizeSurrogates` is a no-op here
//! (Rust strings cannot hold lone surrogates).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::json_repair::{lenient_partial_json, parse_tool_call_arguments};
use super::openai_completions::SseDecoder;
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, Message, StopReason, SystemContent, TextContent,
	TextOrImage, ThinkingContent, ThinkingLevel, ToolCall, Usage, now_ms,
};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// OpenAI Responses rejects `max_output_tokens` below 16
/// (upstream `OPENAI_RESPONSES_MIN_OUTPUT_TOKENS`).
const MIN_OUTPUT_TOKENS: u64 = 16;

/// `prompt_cache_key` max length (upstream `clampOpenAIPromptCacheKey`).
const PROMPT_CACHE_KEY_MAX_CHARS: usize = 64;

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
				message: "Request was aborted".to_string(),
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
/// assistant message when the failure happened mid-stream.
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
	let mut session = ResponsesSession::new(model);
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
		if decoder.is_done() {
			break;
		}
	}

	// Upstream refuses a stream that never reached a terminal response event
	// and tool calls whose `output_item.done` never arrived (their arguments
	// may be cut off). Both surface as stream failures.
	if let Err(message) = session.finish() {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message,
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
	session: &mut ResponsesSession,
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
	let key = options
		.api_key
		.clone()
		.or_else(|| std::env::var("OPENAI_API_KEY").ok())
		.filter(|s| !s.is_empty())
		.ok_or_else(|| "OPENAI_API_KEY not set".to_string())?;
	let base = if model.base_url.is_empty() {
		"https://api.openai.com/v1"
	} else {
		model.base_url.trim_end_matches('/')
	};
	// Session affinity headers (upstream `createClient`, openai format).
	let mut request = reqwest::Client::new()
		.post(format!("{base}/responses"))
		.bearer_auth(key);
	if let Some(session_id) = &options.session_id {
		request = request
			.header("session_id", session_id)
			.header("x-client-request-id", session_id);
	}
	let response = request
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

fn effort_string(level: ThinkingLevel) -> &'static str {
	match level {
		ThinkingLevel::Off => "none",
		ThinkingLevel::Minimal => "minimal",
		ThinkingLevel::Low => "low",
		ThinkingLevel::Medium => "medium",
		ThinkingLevel::High => "high",
		ThinkingLevel::Xhigh => "xhigh",
		ThinkingLevel::Max => "max",
	}
}

/// Text of a system message (content plus nothing else — the port carries no
/// `sections`), joined the way `getSystemMessageText` would.
fn system_text(content: &SystemContent) -> String {
	match content {
		SystemContent::Text(text) => text.clone(),
		SystemContent::Blocks(blocks) => blocks
			.iter()
			.map(|block| block.text.as_str())
			.collect::<Vec<_>>()
			.join("\n"),
	}
}

/// Tool result output: plain text when there are no images (or the model
/// cannot see them), otherwise `input_text`/`input_image` blocks (upstream
/// `convertToolResultOutput`).
fn tool_result_output(model: &Model, content: &[TextOrImage]) -> Value {
	let text = content
		.iter()
		.filter_map(|block| block.text_content())
		.collect::<Vec<_>>()
		.join("\n");
	let images: Vec<&crate::types::ImageContent> = content
		.iter()
		.filter_map(|block| match block {
			TextOrImage::Image(image) => Some(image),
			_ => None,
		})
		.collect();
	let has_text = !text.trim().is_empty();
	if images.is_empty() || !model.supports_images() {
		return json!(if has_text {
			text
		} else if !images.is_empty() {
			"(see attached image)".to_string()
		} else {
			"(no tool output)".to_string()
		});
	}
	let mut output = Vec::new();
	if has_text {
		output.push(json!({"type": "input_text", "text": text}));
	}
	for image in images {
		output.push(json!({
			"type": "input_image",
			"detail": "auto",
			"image_url": format!("data:{};base64,{}", image.mime_type, image.data),
		}));
	}
	json!(output)
}

/// Replays a persisted `text_signature` (`{v:1,id,phase}` JSON) into its id
/// and optional phase; plain legacy signatures are the id itself (upstream
/// `parseTextSignature`).
fn parse_text_signature(signature: &str) -> (Option<String>, Option<String>) {
	if let Ok(value) = serde_json::from_str::<Value>(signature)
		&& value.get("v").and_then(Value::as_i64) == Some(1)
		&& let Some(id) = value.get("id").and_then(Value::as_str)
	{
		let phase = value
			.get("phase")
			.and_then(Value::as_str)
			.filter(|p| matches!(*p, "commentary" | "final_answer"))
			.map(str::to_string);
		return (Some(id.to_string()), phase);
	}
	(Some(signature.to_string()), None)
}

/// JSON of a text block's signature (upstream `encodeTextSignatureV1`).
fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
	let mut value = json!({"v": 1, "id": id});
	if let Some(phase) = phase {
		value["phase"] = json!(phase);
	}
	value.to_string()
}

/// Upstream `normalizeIdPart`: non-alphanumerics collapse to `_`, capped at
/// 64 chars, trailing underscores stripped.
fn normalize_id_part(part: &str) -> String {
	let sanitized: String = part
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
				c
			} else {
				'_'
			}
		})
		.collect();
	let mut normalized: String = sanitized.chars().take(64).collect();
	while normalized.ends_with('_') {
		normalized.pop();
	}
	normalized
}

fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	// No mid-conversation system messages (default compat): all system
	// messages collapse into one leading item carrying the merged prompt
	// text and the current tool set (upstream `collapseSystemMessages` +
	// `getCurrentTools`).
	let mut system_parts: Vec<String> = Vec::new();
	let mut tools_by_name: Vec<crate::types::Tool> = Vec::new();
	for message in &context.messages {
		if let Message::System(system) = message {
			let text = system_text(&system.content);
			if !text.is_empty() {
				system_parts.push(text);
			}
			if let Some(added) = &system.tools_added {
				for tool in added {
					tools_by_name.retain(|existing| existing.name != tool.name);
					tools_by_name.push(tool.clone());
				}
			}
			if let Some(removed) = &system.tools_removed {
				for tool in removed {
					tools_by_name.retain(|existing| existing.name != tool.name);
				}
			}
		}
	}

	let instruction_role = if model.reasoning {
		"developer"
	} else {
		"system"
	};
	let mut input: Vec<Value> = Vec::new();
	if !system_parts.is_empty() {
		input.push(json!({"role": instruction_role, "content": system_parts.join("\n\n")}));
	}

	let mut message_index = 0usize;
	for message in &context.messages {
		match message {
			Message::System(_) => {}
			Message::User(user) => {
				let content: Vec<Value> = match &user.content {
					crate::types::UserContent::Text(text) => {
						vec![json!({"type": "input_text", "text": text})]
					}
					crate::types::UserContent::Blocks(blocks) => blocks
						.iter()
						.map(|block| match block {
							TextOrImage::Text(text) => {
								json!({"type": "input_text", "text": text.text})
							}
							TextOrImage::Image(image) => json!({
								"type": "input_image",
								"detail": "auto",
								"image_url": format!("data:{};base64,{}", image.mime_type, image.data),
							}),
						})
						.collect(),
				};
				if !content.is_empty() {
					input.push(json!({"role": "user", "content": content}));
				}
				message_index += 1;
			}
			Message::Assistant(assistant) => {
				let mut output: Vec<Value> = Vec::new();
				let mut text_block_index = 0usize;
				for block in &assistant.content {
					match block {
						AssistantContent::Thinking(thinking) => {
							// Replay the persisted reasoning item verbatim.
							if let Some(signature) = &thinking.thinking_signature
								&& let Ok(item) = serde_json::from_str::<Value>(signature)
							{
								output.push(item);
							}
						}
						AssistantContent::Text(text) => {
							// OpenAI caps ids at 64 characters.
							let (signature_id, phase) = text
								.text_signature
								.as_deref()
								.map(parse_text_signature)
								.unwrap_or((None, None));
							let fallback = if text_block_index == 0 {
								format!("msg_pi_{message_index}")
							} else {
								format!("msg_pi_{message_index}_{text_block_index}")
							};
							text_block_index += 1;
							let id = match signature_id {
								Some(id) if id.chars().count() > 64 => {
									format!("msg_pi_{message_index}_{text_block_index}")
								}
								Some(id) => id,
								None => fallback,
							};
							let mut item = json!({
								"type": "message",
								"role": "assistant",
								"content": [{"type": "output_text", "text": text.text, "annotations": []}],
								"status": "completed",
								"id": id,
							});
							if let Some(phase) = &phase {
								item["phase"] = json!(phase);
							}
							output.push(item);
						}
						AssistantContent::ToolCall(tool_call) => {
							let (call_id, item_id) = match tool_call.id.split_once('|') {
								Some((call_id, item_id)) => (call_id, Some(item_id)),
								None => (tool_call.id.as_str(), None),
							};
							// Item ids must be `fc_*` for function_call items;
							// anything else (foreign or cross-provider) omits
							// the id, which skips the pairing validation.
							let item_id = item_id
								.map(normalize_id_part)
								.filter(|id| id.starts_with("fc_"));
							let mut item = json!({
								"type": "function_call",
								"call_id": normalize_id_part(call_id),
								"name": tool_call.name,
								"arguments": tool_call.arguments.to_string(),
							});
							if let Some(item_id) = item_id {
								item["id"] = json!(item_id);
							}
							if let Some(namespace) = &tool_call.namespace {
								item["namespace"] = json!(namespace);
							}
							output.push(item);
						}
					}
				}
				if !output.is_empty() {
					input.extend(output);
				}
				message_index += 1;
			}
			Message::ToolResult(result) => {
				let call_id = result
					.tool_call_id
					.split('|')
					.next()
					.unwrap_or(&result.tool_call_id);
				input.push(json!({
					"type": "function_call_output",
					"call_id": normalize_id_part(call_id),
					"output": tool_result_output(model, &result.content),
				}));
			}
		}
	}

	let mut body = json!({
		"model": model.id,
		"input": input,
		"stream": true,
		"store": false,
	});
	if let Some(session_id) = &options.session_id {
		let clamped: String = session_id
			.chars()
			.take(PROMPT_CACHE_KEY_MAX_CHARS)
			.collect();
		body["prompt_cache_key"] = json!(clamped);
	}
	if let Some(tokens) = options.max_tokens {
		body["max_output_tokens"] = json!(tokens.max(MIN_OUTPUT_TOKENS));
	}
	if let Some(temperature) = options.temperature {
		body["temperature"] = json!(temperature);
	}
	if !tools_by_name.is_empty() {
		body["tools"] = json!(
			tools_by_name
				.iter()
				.map(|tool| json!({
					"type": "function",
					"name": tool.name,
					"description": tool.description,
					"parameters": tool.parameters,
				}))
				.collect::<Vec<_>>()
		);
	}
	// Reasoning: an explicit level sends the effort with summary auto and the
	// encrypted-content include; `off` sends `effort: "none"` unless the
	// model disables it via its (unported) thinking level map.
	match options.reasoning {
		Some(ThinkingLevel::Off) | None => {
			if model.reasoning {
				body["reasoning"] = json!({"effort": "none"});
			}
		}
		Some(level) => {
			body["reasoning"] = json!({"effort": effort_string(level), "summary": "auto"});
			body["include"] = json!(["reasoning.encrypted_content"]);
		}
	}
	body
}

// ---------------------------------------------------------------------------
// Stream processing (processResponsesStream)
// ---------------------------------------------------------------------------

/// Raw argument text of one streaming tool call, with the tolerant partial
/// preview (upstream `ToolCallArgumentsStream`: append consumes each piece
/// once, `replace` continues or restarts, `finish` does the one full parse).
struct ToolArgsStream {
	text: String,
	arguments: Value,
}

impl ToolArgsStream {
	fn new() -> Self {
		Self {
			text: String::new(),
			arguments: json!({}),
		}
	}

	fn append(&mut self, delta: &str) -> Value {
		self.text.push_str(delta);
		self.arguments = lenient_partial_json(&self.text);
		self.arguments.clone()
	}

	/// Returns the missing piece when `text` continues the received text;
	/// `None` on restart or when nothing new arrived.
	fn replace(&mut self, text: &str) -> Option<String> {
		if text.starts_with(&self.text) {
			let delta = text[self.text.len()..].to_string();
			if delta.is_empty() {
				return None;
			}
			self.append(&delta);
			return Some(delta);
		}
		self.text.clear();
		self.arguments = json!({});
		self.append(text);
		None
	}

	fn finish(&self) -> (Value, Option<String>) {
		parse_tool_call_arguments(&self.text)
	}
}

/// Output slots keyed by `output_index`; a slot lives from
/// `output_item.added` to `output_item.done` (upstream `ResponsesOutputSlot`).
enum Slot {
	Thinking {
		content_index: usize,
	},
	Text {
		content_index: usize,
	},
	ToolCall {
		content_index: usize,
		args: ToolArgsStream,
	},
}

/// Accumulated assistant message + event construction, mirroring the TS
/// closure state in `processResponsesStream`.
struct ResponsesSession {
	message: AssistantMessage,
	slots: HashMap<u64, Slot>,
	/// Tool calls whose `output_item.done` has not arrived yet.
	unfinished_tool_calls: Vec<String>,
	saw_terminal_response: bool,
}

impl ResponsesSession {
	fn new(model: &Model) -> Self {
		let message = AssistantMessage {
			content: Vec::new(),
			api: model.api.0.clone(),
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
		};
		Self {
			message,
			slots: HashMap::new(),
			unfinished_tool_calls: Vec::new(),
			saw_terminal_response: false,
		}
	}

	fn partial(&self) -> AssistantMessage {
		self.message.clone()
	}

	fn into_message(self) -> AssistantMessage {
		self.message
	}

	fn push_content(&mut self, content: AssistantContent) -> usize {
		self.message.content.push(content);
		self.message.content.len() - 1
	}

	/// A `phase: "final_answer"` message marks a definitive stop (upstream
	/// `applyMessagePhaseStopReason`).
	fn apply_message_phase(&mut self, item: &Value) {
		if item.get("type").and_then(Value::as_str) == Some("message")
			&& item.get("phase").and_then(Value::as_str) == Some("final_answer")
		{
			self.message.stop_reason = StopReason::Stop;
		}
	}

	fn process_payload(&mut self, payload: &str) -> Result<Vec<Event>, String> {
		let event: Value =
			serde_json::from_str(payload).map_err(|e| format!("malformed SSE payload: {e}"))?;
		let event_type = event
			.get("type")
			.and_then(Value::as_str)
			.ok_or_else(|| "SSE payload without a type".to_string())?
			.to_string();
		match event_type.as_str() {
			"response.created" => {
				if let Some(id) = event["response"]["id"].as_str() {
					self.message.response_id = Some(id.to_string());
				}
				Ok(Vec::new())
			}
			"response.output_item.added" => {
				let output_index = event["output_index"].as_u64().unwrap_or(0);
				let item = event["item"].clone();
				self.create_slot(output_index, &item);
				Ok(Vec::new())
			}
			"response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
				self.thinking_delta(output_index_of(&event), delta_of(&event))
			}
			"response.reasoning_summary_part.done" => {
				self.thinking_delta(output_index_of(&event), Some("\n\n".to_string()))
			}
			"response.output_text.delta" | "response.refusal.delta" => {
				self.text_delta(output_index_of(&event), delta_of(&event))
			}
			"response.function_call_arguments.delta" => {
				let output_index = output_index_of(&event);
				let Some(delta) = delta_of(&event) else {
					return Ok(Vec::new());
				};
				let Some((content_index, _)) = self.tool_args_delta(output_index, &delta) else {
					return Ok(Vec::new());
				};
				Ok(vec![self.tool_call_delta_event(content_index, delta)])
			}
			"response.function_call_arguments.done" => {
				let output_index = output_index_of(&event);
				let full = event["arguments"].as_str().unwrap_or_default().to_string();
				let (content_index, delta) =
					self.tool_args_replace(output_index, &full).ok_or_else(|| {
						"function_call_arguments.done without an open slot".to_string()
					})?;
				let mut events = Vec::new();
				if let Some(delta) = delta {
					events.push(self.tool_call_delta_event(content_index, delta));
				}
				Ok(events)
			}
			"response.output_item.done" => {
				let output_index = output_index_of(&event);
				let item = event["item"].clone();
				self.finish_item(output_index, &item)
			}
			"response.completed" | "response.incomplete" => {
				self.saw_terminal_response = true;
				self.finalize_response(&event["response"]);
				Ok(Vec::new())
			}
			"response.failed" => {
				self.saw_terminal_response = true;
				self.message.raw_stop_reason =
					event["response"]["status"].as_str().map(str::to_string);
				let error = &event["response"]["error"];
				let details = &event["response"]["incomplete_details"];
				let message = if let Some(message) = error["message"].as_str() {
					format!("{}: {message}", error["code"].as_str().unwrap_or("unknown"))
				} else if let Some(reason) = details["reason"].as_str() {
					format!("incomplete: {reason}")
				} else {
					"Unknown error (no error details in response)".to_string()
				};
				Err(message)
			}
			"error" => Err(format!(
				"Error Code {}: {}",
				event["code"].as_str().unwrap_or("unknown"),
				event["message"].as_str().unwrap_or("no message")
			)),
			// Ignored: ping/heartbeat and event kinds the port does not act on.
			_ => Ok(Vec::new()),
		}
	}

	fn thinking_delta(
		&mut self,
		output_index: u64,
		delta: Option<String>,
	) -> Result<Vec<Event>, String> {
		let Some(delta) = delta else {
			return Ok(Vec::new());
		};
		let Some(Slot::Thinking { content_index }) = self.slots.get(&output_index) else {
			return Ok(Vec::new());
		};
		let content_index = *content_index;
		let Some(AssistantContent::Thinking(block)) = self.message.content.get_mut(content_index)
		else {
			return Ok(Vec::new());
		};
		block.thinking.push_str(&delta);
		Ok(vec![Event::ThinkingDelta {
			content_index,
			delta,
			partial: self.partial(),
		}])
	}

	fn text_delta(
		&mut self,
		output_index: u64,
		delta: Option<String>,
	) -> Result<Vec<Event>, String> {
		let Some(delta) = delta else {
			return Ok(Vec::new());
		};
		let Some(Slot::Text { content_index }) = self.slots.get(&output_index) else {
			return Ok(Vec::new());
		};
		let content_index = *content_index;
		let Some(AssistantContent::Text(block)) = self.message.content.get_mut(content_index)
		else {
			return Ok(Vec::new());
		};
		block.text.push_str(&delta);
		Ok(vec![Event::TextDelta {
			content_index,
			delta,
			partial: self.partial(),
		}])
	}

	/// Appends raw argument text to the slot's stream; returns the content
	/// index and the emitted arguments.
	fn tool_args_delta(&mut self, output_index: u64, delta: &str) -> Option<(usize, Value)> {
		if let Some(Slot::ToolCall {
			content_index,
			args,
		}) = self.slots.get_mut(&output_index)
		{
			let content_index = *content_index;
			let arguments = args.append(delta);
			if let AssistantContent::ToolCall(call) = self.message.content.get_mut(content_index)? {
				call.arguments = arguments.clone();
			}
			Some((content_index, arguments))
		} else {
			None
		}
	}

	fn tool_args_replace(
		&mut self,
		output_index: u64,
		full: &str,
	) -> Option<(usize, Option<String>)> {
		if let Some(Slot::ToolCall {
			content_index,
			args,
		}) = self.slots.get_mut(&output_index)
		{
			let content_index = *content_index;
			let delta = args.replace(full);
			let arguments = args.arguments.clone();
			if let AssistantContent::ToolCall(call) = self.message.content.get_mut(content_index)? {
				call.arguments = arguments;
			}
			Some((content_index, delta))
		} else {
			None
		}
	}

	fn tool_call_delta_event(&self, content_index: usize, delta: String) -> Event {
		Event::ToolCallDelta {
			content_index,
			delta,
			partial: self.partial(),
		}
	}

	fn create_slot(&mut self, output_index: u64, item: &Value) {
		match item.get("type").and_then(Value::as_str) {
			Some("reasoning") => {
				let content_index =
					self.push_content(AssistantContent::Thinking(ThinkingContent {
						thinking: String::new(),
						thinking_signature: None,
						redacted: None,
					}));
				self.slots
					.insert(output_index, Slot::Thinking { content_index });
			}
			Some("message") => {
				self.apply_message_phase(item);
				let content_index = self.push_content(AssistantContent::Text(TextContent {
					text: String::new(),
					text_signature: None,
				}));
				self.slots
					.insert(output_index, Slot::Text { content_index });
			}
			Some("function_call") => {
				let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
				let item_id = item["id"].as_str().unwrap_or_default();
				let name = item["name"].as_str().unwrap_or_default().to_string();
				let mut args = ToolArgsStream::new();
				if let Some(initial) = item["arguments"].as_str() {
					args.append(initial);
				}
				let composite_id = format!("{call_id}|{item_id}");
				let call = ToolCall {
					id: composite_id.clone(),
					name,
					arguments: args.arguments.clone(),
					thought_signature: None,
					namespace: item["namespace"].as_str().map(str::to_string),
					arguments_error: None,
				};
				let content_index = self.push_content(AssistantContent::ToolCall(call));
				self.unfinished_tool_calls.push(composite_id);
				self.slots.insert(
					output_index,
					Slot::ToolCall {
						content_index,
						args,
					},
				);
			}
			// custom_tool_call and unknown item types create no slot.
			_ => {}
		}
	}

	fn finish_item(&mut self, output_index: u64, item: &Value) -> Result<Vec<Event>, String> {
		self.apply_message_phase(item);
		let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
		match item_type {
			"reasoning" => {
				let Some(Slot::Thinking { content_index }) = self.slots.remove(&output_index)
				else {
					return Ok(Vec::new());
				};
				let summary = item["summary"]
					.as_array()
					.map(|parts| {
						parts
							.iter()
							.filter_map(|part| part["text"].as_str())
							.collect::<Vec<_>>()
							.join("\n\n")
					})
					.unwrap_or_default();
				let content = item["content"]
					.as_array()
					.map(|parts| {
						parts
							.iter()
							.filter_map(|part| part["text"].as_str())
							.collect::<Vec<_>>()
							.join("\n\n")
					})
					.unwrap_or_default();
				let Some(AssistantContent::Thinking(block)) =
					self.message.content.get_mut(content_index)
				else {
					return Ok(Vec::new());
				};
				if !summary.is_empty() {
					block.thinking = summary;
				} else if !content.is_empty() {
					block.thinking = content;
				}
				block.thinking_signature = Some(item.to_string());
				let text = block.thinking.clone();
				Ok(vec![Event::ThinkingEnd {
					content_index,
					content: text,
					partial: self.partial(),
				}])
			}
			"message" => {
				let Some(Slot::Text { content_index }) = self.slots.remove(&output_index) else {
					return Ok(Vec::new());
				};
				let text = item["content"]
					.as_array()
					.map(|parts| {
						parts
							.iter()
							.filter_map(|part| {
								part["text"].as_str().or_else(|| part["refusal"].as_str())
							})
							.collect::<Vec<_>>()
							.join("")
					})
					.unwrap_or_default();
				let Some(AssistantContent::Text(block)) =
					self.message.content.get_mut(content_index)
				else {
					return Ok(Vec::new());
				};
				block.text = text;
				let id = item["id"].as_str().unwrap_or_default();
				block.text_signature = Some(encode_text_signature(id, item["phase"].as_str()));
				let text = block.text.clone();
				Ok(vec![Event::TextEnd {
					content_index,
					content: text,
					partial: self.partial(),
				}])
			}
			"function_call" => {
				let Some(Slot::ToolCall {
					content_index,
					args,
				}) = self.slots.remove(&output_index)
				else {
					return Ok(Vec::new());
				};
				let mut args = args;
				if let Some(full) = item["arguments"].as_str() {
					args.replace(full);
				}
				let (arguments, arguments_error) = args.finish();
				let Some(AssistantContent::ToolCall(call)) =
					self.message.content.get_mut(content_index)
				else {
					return Ok(Vec::new());
				};
				call.arguments = arguments;
				call.arguments_error = arguments_error;
				if let Some(namespace) = item["namespace"].as_str() {
					call.namespace = Some(namespace.to_string());
				}
				self.unfinished_tool_calls.retain(|id| id != &call.id);
				let call = call.clone();
				Ok(vec![Event::ToolCallEnd {
					content_index,
					tool_call: call,
					partial: self.partial(),
				}])
			}
			_ => Ok(Vec::new()),
		}
	}

	/// Usage, stop reason and cost mapping from the terminal response object
	/// (upstream `finalizeResponse`).
	fn finalize_response(&mut self, response: &Value) {
		if let Some(id) = response["id"].as_str() {
			self.message.response_id = Some(id.to_string());
		}
		if let Some(usage) = response.get("usage") {
			let cached = usage["input_tokens_details"]["cached_tokens"]
				.as_u64()
				.unwrap_or(0);
			let cache_write = usage["input_tokens_details"]["cache_write_tokens"]
				.as_u64()
				.unwrap_or(0);
			let input_total = usage["input_tokens"].as_u64().unwrap_or(0);
			self.message.usage = Usage {
				// OpenAI includes cached and cache-write tokens in input_tokens.
				input: input_total
					.saturating_sub(cached)
					.saturating_sub(cache_write),
				output: usage["output_tokens"].as_u64().unwrap_or(0),
				cache_read: cached,
				cache_write,
				cache_write_1h: None,
				reasoning: Some(
					usage["output_tokens_details"]["reasoning_tokens"]
						.as_u64()
						.unwrap_or(0),
				)
				.filter(|tokens| *tokens > 0),
				total_tokens: usage["total_tokens"].as_u64().unwrap_or(0),
				cost: Default::default(),
			};
		}
		let status = response["status"].as_str().unwrap_or_default().to_string();
		let incomplete_reason = response["incomplete_details"]["reason"]
			.as_str()
			.map(str::to_string);
		self.message.raw_stop_reason = Some(match &incomplete_reason {
			Some(reason) => format!("{status}.{reason}"),
			None => status.clone(),
		});
		let (stop_reason, error_message) = map_stop_reason(&status, incomplete_reason.as_deref());
		self.message.stop_reason = stop_reason;
		self.message.error_message = error_message;
		if self.message.stop_reason == StopReason::Stop
			&& self
				.message
				.content
				.iter()
				.any(|block| matches!(block, AssistantContent::ToolCall(_)))
		{
			self.message.stop_reason = StopReason::ToolUse;
		}
	}

	/// Terminal checks (upstream: the post-loop guards in
	/// `processResponsesStream`). `Err` carries the failure message.
	fn finish(&self) -> Result<(), String> {
		if !self.saw_terminal_response {
			return Err(
				"OpenAI Responses stream ended before a terminal response event".to_string(),
			);
		}
		if self.message.stop_reason == StopReason::ToolUse && !self.unfinished_tool_calls.is_empty()
		{
			let pending = self
				.unfinished_tool_calls
				.iter()
				.map(|id| {
					self.message
						.content
						.iter()
						.find_map(|block| match block {
							AssistantContent::ToolCall(call) if call.id == *id => {
								Some(format!("{} ({})", call.name, call.id))
							}
							_ => None,
						})
						.unwrap_or_else(|| id.clone())
				})
				.collect::<Vec<_>>()
				.join(", ");
			return Err(format!(
				"OpenAI Responses stream completed with an unfinished tool call: {pending}"
			));
		}
		Ok(())
	}
}

fn output_index_of(event: &Value) -> u64 {
	event["output_index"].as_u64().unwrap_or(0)
}

fn delta_of(event: &Value) -> Option<String> {
	event["delta"].as_str().map(str::to_string)
}

fn map_stop_reason(status: &str, incomplete_reason: Option<&str>) -> (StopReason, Option<String>) {
	match status {
		"completed" => (StopReason::Stop, None),
		"incomplete" => match incomplete_reason {
			Some("max_output_tokens") => (StopReason::Length, None),
			Some(reason) => (
				StopReason::Error,
				Some(format!("Response incomplete: {reason}")),
			),
			None => (
				StopReason::Error,
				Some("Response incomplete without a provider reason".to_string()),
			),
		},
		// "failed"/"cancelled" map to error; "in_progress"/"queued" are
		// upstream's "wonky" stop-as-stop statuses.
		"failed" | "cancelled" => (StopReason::Error, None),
		"in_progress" | "queued" => (StopReason::Stop, None),
		"" => (StopReason::Stop, None),
		other => (
			StopReason::Error,
			Some(format!("Unhandled response status: {other}")),
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::stream::normalize_context;
	use crate::types::Tool;
	use crate::types::{SystemMessage, ToolResultMessage};

	fn test_model() -> Model {
		Model::new("gpt-test", "gpt-test", "openai-responses", "openai", "")
	}

	fn reasoning_model() -> Model {
		let mut model = test_model();
		model.reasoning = true;
		model
	}

	fn tools_system() -> SystemMessage {
		let mut system = SystemMessage::prompt("be brief");
		system.tools_added = Some(vec![Tool::new(
			"ls",
			"list files",
			json!({"type": "object", "properties": {"path": {"type": "string"}}}),
		)]);
		system
	}

	#[test]
	fn request_body_wire_shape() {
		let model = test_model();
		let ctx = TranscriptContext {
			messages: vec![
				Message::System(tools_system()),
				Message::user("list the files"),
			],
		};
		let options = StreamOptions {
			max_tokens: Some(8),
			session_id: Some("sess-123".into()),
			..Default::default()
		};
		let body = request_body(&model, &ctx, &options);
		assert_eq!(body["model"], "gpt-test");
		assert_eq!(body["stream"], true);
		assert_eq!(body["store"], false);
		assert_eq!(body["prompt_cache_key"], "sess-123");
		assert_eq!(body["max_output_tokens"], 16, "clamped to the minimum");
		// System collapses to the head item; non-reasoning models use role system.
		assert_eq!(body["input"][0]["role"], "system");
		assert_eq!(body["input"][0]["content"], "be brief");
		assert_eq!(body["input"][1]["role"], "user");
		assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
		// Function tools carry no strict flag (compat off).
		assert_eq!(body["tools"][0]["type"], "function");
		assert_eq!(body["tools"][0]["name"], "ls");
		assert!(body["tools"][0].get("strict").is_none());
		assert!(
			body.get("reasoning").is_none(),
			"non-reasoning model sends none"
		);
		assert!(body.get("include").is_none());
	}

	#[test]
	fn reasoning_level_maps_to_effort_and_include() {
		let model = reasoning_model();
		let ctx = normalize_context(None, &[], vec![Message::user("hi")]);
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::High),
			..Default::default()
		};
		let body = request_body(&model, &ctx, &options);
		assert_eq!(body["reasoning"]["effort"], "high");
		assert_eq!(body["reasoning"]["summary"], "auto");
		assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
		// Reasoning models use the developer role for the system item.
		let ctx = normalize_context(Some("be brief"), &[], vec![Message::user("hi")]);
		let body = request_body(&model, &ctx, &options);
		assert_eq!(body["input"][0]["role"], "developer");
	}

	#[test]
	fn reasoning_off_sends_none_effort() {
		let model = reasoning_model();
		let ctx = normalize_context(None, &[], vec![Message::user("hi")]);
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(body["reasoning"]["effort"], "none");
	}

	#[test]
	fn assistant_history_replays_items() {
		let model = test_model();
		let assistant = AssistantMessage {
			content: vec![
				AssistantContent::Thinking(ThinkingContent {
					thinking: "reasoned".into(),
					thinking_signature: Some(r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"reasoned"}]}"#.into()),
					redacted: None,
				}),
				AssistantContent::Text(TextContent {
					text: "here you go".into(),
					text_signature: Some(r#"{"v":1,"id":"msg_abc","phase":"final_answer"}"#.into()),
				}),
				AssistantContent::ToolCall(ToolCall {
					id: "call_7|fc_9".into(),
					name: "ls".into(),
					arguments: json!({"path": "."}),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				}),
				// A plain (non-pipe) id from a foreign provider: item id omitted.
				AssistantContent::ToolCall(ToolCall {
					id: "call_8".into(),
					name: "cat".into(),
					arguments: json!({}),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				}),
			],
			..AssistantMessage::error(&model, StopReason::Pending, "")
		};
		let ctx = TranscriptContext {
			messages: vec![
				Message::user("go"),
				Message::Assistant(assistant),
				Message::ToolResult(ToolResultMessage::text_result(
					"call_7|fc_9",
					"ls",
					"a.txt",
					false,
				)),
				Message::ToolResult(ToolResultMessage::text_result(
					"call_8", "cat", "b.txt", false,
				)),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let input = body["input"].as_array().unwrap();
		assert_eq!(input[1]["type"], "reasoning", "signature replayed verbatim");
		assert_eq!(input[1]["id"], "rs_1");
		assert_eq!(input[2]["type"], "message");
		assert_eq!(input[2]["id"], "msg_abc", "signature id reused");
		assert_eq!(input[2]["phase"], "final_answer");
		assert_eq!(input[2]["content"][0]["type"], "output_text");
		assert_eq!(input[3]["type"], "function_call");
		assert_eq!(input[3]["call_id"], "call_7");
		assert_eq!(input[3]["id"], "fc_9", "fc_-prefixed item id kept");
		assert_eq!(input[3]["arguments"], r#"{"path":"."}"#);
		assert_eq!(input[4]["type"], "function_call");
		assert_eq!(input[4]["call_id"], "call_8");
		assert!(input[4].get("id").is_none(), "foreign id omitted");
		assert_eq!(input[5]["type"], "function_call_output");
		assert_eq!(input[5]["call_id"], "call_7");
		assert_eq!(input[5]["output"], "a.txt");
		assert_eq!(input[6]["output"], "b.txt");
	}

	fn fresh_session() -> ResponsesSession {
		ResponsesSession::new(&test_model())
	}

	#[test]
	fn stream_maps_text_thinking_and_tool_events() {
		let mut session = fresh_session();
		session
			.process_payload(r#"{"type":"response.created","response":{"id":"resp_1"}}"#)
			.unwrap();
		assert_eq!(session.message.response_id.as_deref(), Some("resp_1"));

		// Reasoning block opens, streams, and closes with its raw item as
		// the replay signature.
		session
			.process_payload(
				r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thinking"}"#,
			)
			.unwrap();
		session
			.process_payload(r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"done thinking"}],"encrypted_content":"enc"}}"#,
			)
			.unwrap();
		assert_eq!(session.message.content.len(), 1);
		let AssistantContent::Thinking(thinking) = &session.message.content[0] else {
			panic!("thinking block");
		};
		assert_eq!(thinking.thinking, "done thinking");
		assert!(
			thinking
				.thinking_signature
				.as_deref()
				.is_some_and(|s| s.contains("encrypted_content"))
		);

		// Text block streams to a completed output message.
		session
			.process_payload(
				r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","phase":"final_answer"}}"#,
			)
			.unwrap();
		assert_eq!(
			session.message.stop_reason,
			StopReason::Stop,
			"final_answer phase"
		);
		session
			.process_payload(
				r#"{"type":"response.output_text.delta","output_index":1,"delta":"hello "}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.output_text.delta","output_index":1,"delta":"world"}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"hello world"}],"phase":"final_answer"}}"#,
			)
			.unwrap();
		let AssistantContent::Text(text) = &session.message.content[1] else {
			panic!("text block");
		};
		assert_eq!(text.text, "hello world");
		assert!(
			text.text_signature
				.as_deref()
				.is_some_and(|s| s.contains("msg_1"))
		);

		// Function call: added + argument deltas + done replaces and finishes.
		session
			.process_payload(
				r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"ls","arguments":""}}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":"}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"\".\"}"}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"path\":\".\"}"}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"ls","arguments":"{\"path\":\".\"}"}}"#,
			)
			.unwrap();
		let AssistantContent::ToolCall(call) = &session.message.content[2] else {
			panic!("tool call block");
		};
		assert_eq!(call.id, "call_1|fc_1");
		assert_eq!(call.arguments, json!({"path": "."}));
		assert!(session.unfinished_tool_calls.is_empty());

		// Terminal response: usage subtracts cache tokens from input.
		session
			.process_payload(
				r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":120,"output_tokens":30,"total_tokens":150,"input_tokens_details":{"cached_tokens":20},"output_tokens_details":{"reasoning_tokens":10}}}}"#,
			)
			.unwrap();
		assert_eq!(session.message.usage.input, 100, "cached tokens subtracted");
		assert_eq!(session.message.usage.cache_read, 20);
		assert_eq!(session.message.usage.output, 30);
		assert_eq!(session.message.usage.reasoning, Some(10));
		assert_eq!(session.message.usage.total_tokens, 150);
		assert_eq!(session.message.stop_reason, StopReason::ToolUse, "upgraded");
		assert!(session.finish().is_ok());
	}

	#[test]
	fn incomplete_response_maps_max_output_tokens_to_length() {
		let mut session = fresh_session();
		session
			.process_payload(
				r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.output_text.delta","output_index":0,"delta":"cut off"}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}"#,
			)
			.unwrap();
		assert_eq!(session.message.stop_reason, StopReason::Length);
		assert_eq!(
			session.message.raw_stop_reason.as_deref(),
			Some("incomplete.max_output_tokens")
		);
		assert!(session.finish().is_ok());
	}

	#[test]
	fn response_failed_and_error_events_are_failures() {
		let mut session = fresh_session();
		assert!(session
			.process_payload(
				r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
			)
			.is_err());
		let mut session = fresh_session();
		assert!(
			session
				.process_payload(r#"{"type":"error","code":"rate_limit","message":"slow down"}"#)
				.is_err()
		);
	}

	#[test]
	fn stream_without_terminal_event_fails() {
		let session = fresh_session();
		assert_eq!(
			session.finish().unwrap_err(),
			"OpenAI Responses stream ended before a terminal response event"
		);
	}

	#[test]
	fn unfinished_tool_calls_are_refused() {
		let mut session = fresh_session();
		session
			.process_payload(
				r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"ls","arguments":"{\"path\":"}}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
			)
			.unwrap();
		assert_eq!(session.message.stop_reason, StopReason::ToolUse);
		assert!(
			session
				.finish()
				.unwrap_err()
				.contains("unfinished tool call: ls (call_1|fc_1)")
		);
	}

	#[test]
	fn tool_result_output_uses_image_blocks_for_vision_models() {
		let mut model = test_model();
		model.input = vec![
			crate::model::InputModality::Text,
			crate::model::InputModality::Image,
		];
		let content = vec![
			TextOrImage::text("see attachment"),
			TextOrImage::Image(crate::types::ImageContent {
				data: "aGVsbG8=".into(),
				mime_type: "image/png".into(),
			}),
		];
		let output = tool_result_output(&model, &content);
		assert!(output.is_array());
		assert_eq!(output[0]["type"], "input_text");
		assert_eq!(output[1]["type"], "input_image");
		// Text-only models get the plain string form.
		let text_model = test_model();
		let output = tool_result_output(&text_model, &content);
		assert_eq!(output, json!("see attachment"));
	}
}
