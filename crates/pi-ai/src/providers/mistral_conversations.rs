//! Mistral Conversations adapter — port of `pi/packages/ai/src/api/
//! mistral-conversations.ts` (first slice): the native Mistral Chat
//! Completions wire (camelCase types in the TS model layer, snake_case on
//! the wire) with thinking content chunks, prefix-less assistant replay,
//! and index-keyed streaming tool calls.
//!
//! Deferred: `toolChoice`, `reasoningEffort`/`promptMode` (the port's
//! ThinkingLevel has no per-model thinkingLevelMap yet), the 60s
//! header-wait timeout (other ported adapters share this omission), header
//! overrides, and the tool-call id normalizer's `shortHash` (std hasher —
//! ids only need to be stable within one session, not equal to upstream's).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::openai_completions::SseDecoder;
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, Message, StopReason, SystemContent, TextContent,
	TextOrImage, ThinkingContent, ToolCall, Usage, now_ms,
};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Upstream `MISTRAL_TOOL_CALL_ID_LENGTH`.
const TOOL_CALL_ID_LENGTH: usize = 9;

pub fn stream_fn() -> StreamFn {
	Arc::new(
		|model: &Model,
		 context: &TranscriptContext,
		 options: StreamOptions|
		 -> AssistantMessageEventStream { spawn_stream(model.clone(), context.clone(), options) },
	)
}

fn spawn_stream(
	model: Model,
	context: TranscriptContext,
	options: StreamOptions,
) -> AssistantMessageEventStream {
	let (tx, rx) = tokio::sync::mpsc::channel::<Event>(EVENT_CHANNEL_CAPACITY);
	tokio::spawn(async move {
		let signal = options.signal.clone();
		let result = tokio::select! {
			_ = signal.cancelled() => Err(Box::new(StreamFailure {
				reason: StopReason::Aborted,
				message: "Request was aborted".to_string(),
				partial: None,
			})),
			result = run_stream(&model, &context, &options, &tx) => result,
		};
		if let Err(failure) = result {
			let mut error = match failure.partial {
				Some(message) => message,
				None => AssistantMessage::error(&model, failure.reason, failure.message.clone()),
			};
			error.stop_reason = failure.reason;
			error.error_message = Some(failure.message);
			let _ = tx
				.send(Event::Error {
					reason: failure.reason,
					error,
				})
				.await;
		}
	});
	Box::pin(futures::stream::unfold(rx, |mut rx| async move {
		rx.recv().await.map(|event| (event, rx))
	}))
}

struct StreamFailure {
	reason: StopReason,
	message: String,
	partial: Option<AssistantMessage>,
}

async fn run_stream(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
	tx: &Sender<Event>,
) -> Result<(), Box<StreamFailure>> {
	let mut response = open_response(model, context, options)
		.await
		.map_err(|message| {
			Box::new(StreamFailure {
				reason: StopReason::Error,
				message,
				partial: None,
			})
		})?;
	let mut session = MistralSession::new(model);
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
				for payload in payloads {
					session
						.process_payload(&payload, tx)
						.await
						.map_err(|message| {
							Box::new(StreamFailure {
								reason: StopReason::Error,
								message,
								partial: Some(session.partial()),
							})
						})?;
				}
				break;
			}
			Err(error) => {
				return Err(Box::new(StreamFailure {
					reason: StopReason::Error,
					message: format!("stream read failed: {error}"),
					partial: Some(session.partial()),
				}));
			}
		};
		let mut payloads = Vec::new();
		decoder.feed(&chunk, &mut payloads);
		for payload in payloads {
			session
				.process_payload(&payload, tx)
				.await
				.map_err(|message| {
					Box::new(StreamFailure {
						reason: StopReason::Error,
						message,
						partial: Some(session.partial()),
					})
				})?;
		}
		if decoder.is_done() {
			break;
		}
	}

	// Close the open block and finalize every tool call's arguments
	// (upstream consumeChatStream's tail), then run the terminal checks.
	session.finish();
	if session.message.stop_reason == StopReason::Pending {
		return Err(Box::new(StreamFailure {
			reason: StopReason::Error,
			message: "Mistral stream ended without a finish reason".to_string(),
			partial: Some(session.into_message()),
		}));
	}
	if matches!(
		session.message.stop_reason,
		StopReason::Aborted | StopReason::Error
	) {
		return Err(Box::new(StreamFailure {
			reason: StopReason::Error,
			message: session
				.message
				.error_message
				.clone()
				.unwrap_or_else(|| "An unknown error occurred".to_string()),
			partial: Some(session.into_message()),
		}));
	}
	let done_event = Event::Done {
		reason: session.message.stop_reason,
		message: session.into_message(),
	};
	forward(tx, vec![done_event]).await;
	Ok(())
}

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
		.filter(|s| !s.is_empty())
		.ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
	let base = model.base_url.trim_end_matches('/');
	let mut request = reqwest::Client::new()
		.post(format!("{base}/v1/chat/completions"))
		.bearer_auth(key)
		.header("accept", "text/event-stream");
	// Prompt-cache affinity: the session id rides an x-affinity header
	// unless overridden (upstream buildMistralHeaders).
	if let Some(session_id) = &options.session_id {
		request = request.header("x-affinity", session_id);
	}
	let response = request
		.json(&request_body(model, context, options))
		.send()
		.await
		.map_err(|e| format!("request failed: {e}"))?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		let detail = body.trim().to_string();
		let detail = if detail.is_empty() {
			"request failed".to_string()
		} else {
			let truncated: String = detail.chars().take(4000).collect();
			truncated
		};
		return Err(format!(
			"Mistral API error ({}): {}",
			status.as_u16(),
			detail
		));
	}
	Ok(response)
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

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

/// Current tool set across all system messages (upstream `getCurrentTools`).
fn current_tools(context: &TranscriptContext) -> Vec<crate::types::Tool> {
	let mut tools: Vec<crate::types::Tool> = Vec::new();
	for message in &context.messages {
		if let Message::System(system) = message {
			if let Some(removed) = &system.tools_removed {
				for tool in removed {
					tools.retain(|existing| existing.name != tool.name);
				}
			}
			if let Some(added) = &system.tools_added {
				for tool in added {
					tools.retain(|existing| existing.name != tool.name);
					tools.push(tool.clone());
				}
			}
		}
	}
	tools
}

/// `[tool error]` / image-omission wording (upstream `buildToolResultText`).
fn tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
	let trimmed = text.trim();
	let error_prefix = if is_error { "[tool error] " } else { "" };
	if !trimmed.is_empty() {
		let image_suffix = if has_images && !supports_images {
			"\n[tool image omitted: model does not support images]"
		} else {
			""
		};
		return format!("{error_prefix}{trimmed}{image_suffix}");
	}
	if has_images {
		return if supports_images {
			if is_error {
				"[tool error] (see attached image)".into()
			} else {
				"(see attached image)".into()
			}
		} else if is_error {
			"[tool error] (image omitted: model does not support images)".into()
		} else {
			"(image omitted: model does not support images)".into()
		};
	}
	if is_error {
		"[tool error] (no tool output)".into()
	} else {
		"(no tool output)".into()
	}
}

fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	let supports_images = model.supports_images();
	let mut messages: Vec<Value> = Vec::new();
	for (index, message) in context.messages.iter().enumerate() {
		match message {
			Message::System(system) => {
				let text = system_text(&system.content);
				if !text.is_empty() {
					messages.push(json!({ "role": "system", "content": text }));
				}
			}
			Message::User(user) => match &user.content {
				crate::types::UserContent::Text(text) => {
					messages.push(json!({ "role": "user", "content": text }));
				}
				crate::types::UserContent::Blocks(blocks) => {
					let had_images = blocks
						.iter()
						.any(|block| matches!(block, TextOrImage::Image(_)));
					let content: Vec<Value> = blocks
						.iter()
						.filter(|block| match block {
							TextOrImage::Text(_) => true,
							TextOrImage::Image(_) => supports_images,
						})
						.map(|block| match block {
							TextOrImage::Text(text) => json!({ "type": "text", "text": text.text }),
							TextOrImage::Image(image) => json!({
								"type": "image_url",
								"image_url": format!("data:{};base64,{}", image.mime_type, image.data),
							}),
						})
						.collect();
					if !content.is_empty() {
						messages.push(json!({ "role": "user", "content": content }));
					} else if had_images && !supports_images {
						messages.push(json!({
							"role": "user",
							"content": "(image omitted: model does not support images)",
						}));
					}
				}
			},
			Message::Assistant(assistant) => {
				let mut content_parts: Vec<Value> = Vec::new();
				let mut tool_calls: Vec<Value> = Vec::new();
				for block in &assistant.content {
					match block {
						AssistantContent::Text(text) => {
							if !text.text.trim().is_empty() {
								content_parts.push(json!({ "type": "text", "text": text.text }));
							}
						}
						AssistantContent::Thinking(thinking) => {
							if !thinking.thinking.trim().is_empty() {
								content_parts.push(json!({
									"type": "thinking",
									"thinking": [{"type": "text", "text": thinking.thinking}],
								}));
							}
						}
						AssistantContent::ToolCall(call) => {
							tool_calls.push(json!({
								"id": call.id,
								"type": "function",
								"function": {
									"name": call.name,
									"arguments": call.arguments.to_string(),
								},
								"index": 0,
							}));
						}
					}
				}
				if content_parts.is_empty() && tool_calls.is_empty() {
					continue;
				}
				let mut assistant_message = json!({ "role": "assistant", "prefix": false });
				if !content_parts.is_empty() {
					assistant_message["content"] = json!(content_parts);
				}
				if !tool_calls.is_empty() {
					assistant_message["tool_calls"] = json!(tool_calls);
				}
				messages.push(assistant_message);
			}
			Message::ToolResult(result) => {
				let text = result
					.content
					.iter()
					.filter_map(|block| block.text_content())
					.collect::<Vec<_>>()
					.join("\n");
				let has_images = result
					.content
					.iter()
					.any(|block| matches!(block, TextOrImage::Image(_)));
				let mut content = vec![json!({
					"type": "text",
					"text": tool_result_text(&text, has_images, supports_images, result.is_error),
				})];
				if supports_images {
					for block in &result.content {
						if let TextOrImage::Image(image) = block {
							content.push(json!({
								"type": "image_url",
								"image_url": format!("data:{};base64,{}", image.mime_type, image.data),
							}));
						}
					}
				}
				messages.push(json!({
					"role": "tool",
					"tool_call_id": result.tool_call_id,
					"name": result.tool_name,
					"content": content,
				}));
			}
		}
		let _ = index;
	}

	let mut body = json!({
		"model": model.id,
		"stream": true,
		"messages": messages,
	});
	let tools = current_tools(context);
	if !tools.is_empty() {
		body["tools"] = json!(
			tools
				.iter()
				.map(|tool| json!({
					"type": "function",
					"function": {
						"name": tool.name,
						"description": tool.description,
						"parameters": tool.parameters,
						"strict": false,
					},
				}))
				.collect::<Vec<_>>()
		);
	}
	if let Some(temperature) = options.temperature {
		body["temperature"] = json!(temperature);
	}
	if let Some(max_tokens) = options.max_tokens {
		body["max_tokens"] = json!(max_tokens);
	}
	if let Some(session_id) = &options.session_id {
		body["prompt_cache_key"] = json!(session_id);
	}
	body
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

/// Per-tool-call stream state keyed by the chunk's `index` (or id).
struct OpenToolCall {
	content_index: usize,
	args: super::openai_responses::ToolArgsStream,
}

enum OpenBlock {
	Text { content_index: usize },
	Thinking { content_index: usize },
}

struct MistralSession {
	message: AssistantMessage,
	open_block: Option<OpenBlock>,
	tool_calls_by_key: HashMap<String, OpenToolCall>,
}

impl MistralSession {
	fn new(model: &Model) -> Self {
		Self {
			message: AssistantMessage {
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
			},
			open_block: None,
			tool_calls_by_key: HashMap::new(),
		}
	}

	fn partial(&self) -> AssistantMessage {
		self.message.clone()
	}

	fn into_message(self) -> AssistantMessage {
		self.message
	}

	fn close_open_block(&mut self) -> Option<Event> {
		match self.open_block.take() {
			Some(OpenBlock::Text { content_index }) => {
				let content = match self.message.content.get(content_index) {
					Some(AssistantContent::Text(text)) => text.text.clone(),
					_ => String::new(),
				};
				Some(Event::TextEnd {
					content_index,
					content,
					partial: self.partial(),
				})
			}
			Some(OpenBlock::Thinking { content_index }) => {
				let content = match self.message.content.get(content_index) {
					Some(AssistantContent::Thinking(thinking)) => thinking.thinking.clone(),
					_ => String::new(),
				};
				Some(Event::ThinkingEnd {
					content_index,
					content,
					partial: self.partial(),
				})
			}
			None => None,
		}
	}

	fn open_text(&mut self) -> Event {
		let content_index = self.push_content(AssistantContent::Text(TextContent {
			text: String::new(),
			text_signature: None,
		}));
		self.open_block = Some(OpenBlock::Text { content_index });
		Event::TextStart {
			content_index,
			partial: self.partial(),
		}
	}

	fn open_thinking(&mut self) -> Event {
		let content_index = self.push_content(AssistantContent::Thinking(ThinkingContent {
			thinking: String::new(),
			thinking_signature: None,
			redacted: None,
		}));
		self.open_block = Some(OpenBlock::Thinking { content_index });
		Event::ThinkingStart {
			content_index,
			partial: self.partial(),
		}
	}

	fn push_content(&mut self, content: AssistantContent) -> usize {
		self.message.content.push(content);
		self.message.content.len() - 1
	}

	/// Derive a 9-char alnum tool-call id (upstream `deriveMistralToolCallId`
	/// with the std hasher in place of `shortHash` — session-stable only).
	fn derive_tool_call_id(seed: &str) -> String {
		use std::hash::{Hash, Hasher};
		let normalized: String = seed.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		normalized.hash(&mut hasher);
		let hex = format!("{:016x}", hasher.finish());
		let filtered: String = hex.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
		filtered.chars().take(TOOL_CALL_ID_LENGTH).collect()
	}

	async fn process_payload(&mut self, payload: &str, tx: &Sender<Event>) -> Result<(), String> {
		let event: Value =
			serde_json::from_str(payload).map_err(|e| format!("malformed SSE payload: {e}"))?;
		if event
			.pointer("/data/choices")
			.and_then(Value::as_array)
			.is_none()
		{
			return Err("Invalid Mistral streaming event".to_string());
		}
		let mut events = Vec::new();

		// First non-empty id wins (upstream `responseId ||=`).
		if self.message.response_id.is_none()
			&& let Some(id) = event["data"]["id"].as_str()
			&& !id.is_empty()
		{
			self.message.response_id = Some(id.to_string());
		}

		if let Some(usage) = event["data"].get("usage").filter(|usage| !usage.is_null()) {
			let prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
			let cached = Self::cached_prompt_tokens(usage).min(prompt_tokens);
			self.message.usage = Usage {
				input: prompt_tokens.saturating_sub(cached),
				output: usage["completion_tokens"].as_u64().unwrap_or(0),
				cache_read: cached,
				cache_write: 0,
				cache_write_1h: None,
				reasoning: None,
				total_tokens: usage["total_tokens"].as_u64().unwrap_or(0),
				cost: Default::default(),
			};
		}

		let choice = &event["data"]["choices"][0];
		if choice.is_null() {
			return Ok(());
		}
		if let Some(finish_reason) = choice["finish_reason"].as_str() {
			self.message.raw_stop_reason = Some(finish_reason.to_string());
			let (stop_reason, error_message) = map_chat_stop_reason(finish_reason);
			self.message.stop_reason = stop_reason;
			self.message.error_message = error_message;
		}

		let delta = &choice["delta"];
		if let Some(content) = delta.get("content").filter(|content| !content.is_null()) {
			// content: string | chunk array.
			let items: Vec<Value> = match content {
				Value::String(text) => vec![json!(text)],
				Value::Array(items) => items.clone(),
				_ => Vec::new(),
			};
			for item in items {
				match item {
					Value::String(text_delta) => {
						// Empty deltas around thinking/tool calls must not
						// split blocks (GLM models on Mistral).
						if text_delta.is_empty() {
							continue;
						}
						let switch = !matches!(self.open_block, Some(OpenBlock::Text { .. }));
						if switch {
							if let Some(event) = self.close_open_block() {
								events.push(event);
							}
							events.push(self.open_text());
						}
						if let Some(OpenBlock::Text { content_index }) = self.open_block {
							if let Some(AssistantContent::Text(text)) =
								self.message.content.get_mut(content_index)
							{
								text.text.push_str(&text_delta);
							}
							events.push(Event::TextDelta {
								content_index,
								delta: text_delta,
								partial: self.partial(),
							});
						}
					}
					Value::Object(chunk) => match chunk["type"].as_str() {
						Some("thinking") => {
							let delta_text = chunk["thinking"]
								.as_array()
								.map(|parts| {
									parts
										.iter()
										.filter_map(|part| part["text"].as_str())
										.collect::<String>()
								})
								.unwrap_or_default();
							if delta_text.is_empty() {
								continue;
							}
							let switch =
								!matches!(self.open_block, Some(OpenBlock::Thinking { .. }));
							if switch {
								if let Some(event) = self.close_open_block() {
									events.push(event);
								}
								events.push(self.open_thinking());
							}
							if let Some(OpenBlock::Thinking { content_index }) = self.open_block {
								if let Some(AssistantContent::Thinking(thinking)) =
									self.message.content.get_mut(content_index)
								{
									thinking.thinking.push_str(&delta_text);
								}
								events.push(Event::ThinkingDelta {
									content_index,
									delta: delta_text,
									partial: self.partial(),
								});
							}
						}
						Some("text") => {
							let text_delta = chunk["text"].as_str().unwrap_or_default().to_string();
							if text_delta.is_empty() {
								continue;
							}
							let switch = !matches!(self.open_block, Some(OpenBlock::Text { .. }));
							if switch {
								if let Some(event) = self.close_open_block() {
									events.push(event);
								}
								events.push(self.open_text());
							}
							if let Some(OpenBlock::Text { content_index }) = self.open_block {
								if let Some(AssistantContent::Text(text)) =
									self.message.content.get_mut(content_index)
								{
									text.text.push_str(&text_delta);
								}
								events.push(Event::TextDelta {
									content_index,
									delta: text_delta,
									partial: self.partial(),
								});
							}
						}
						_ => {}
					},
					_ => {}
				}
			}
		}

		// Streaming tool calls, keyed by `index` (or the derived call id).
		if let Some(tool_calls) = delta["tool_calls"].as_array() {
			for tool_call in tool_calls {
				if let Some(event) = self.close_open_block() {
					events.push(event);
				}
				let provided_id = tool_call["id"]
					.as_str()
					.filter(|id| !id.is_empty() && *id != "null");
				let index = tool_call["index"]
					.as_u64()
					.map(|index| index.to_string())
					.unwrap_or_default();
				let call_id = provided_id
					.map(str::to_string)
					.unwrap_or_else(|| Self::derive_tool_call_id(&format!("toolcall:{index}")));
				let key = if tool_call["index"].is_null() {
					call_id.clone()
				} else {
					index
				};
				if !self.tool_calls_by_key.contains_key(&key) {
					let name = tool_call["function"]["name"]
						.as_str()
						.unwrap_or_default()
						.to_string();
					let content_index = self.push_content(AssistantContent::ToolCall(ToolCall {
						id: call_id,
						name,
						arguments: json!({}),
						thought_signature: None,
						namespace: None,
						arguments_error: None,
					}));
					self.tool_calls_by_key.insert(
						key.clone(),
						OpenToolCall {
							content_index,
							args: super::openai_responses::ToolArgsStream::new(),
						},
					);
					events.push(Event::ToolCallStart {
						content_index,
						partial: self.partial(),
					});
				}
				let args_delta = match &tool_call["function"]["arguments"] {
					Value::String(text) => text.clone(),
					other if !other.is_null() => other.to_string(),
					_ => String::new(),
				};
				if let Some(open) = self.tool_calls_by_key.get_mut(&key) {
					let content_index = open.content_index;
					let arguments = open.args.append(&args_delta);
					if let Some(AssistantContent::ToolCall(call)) =
						self.message.content.get_mut(content_index)
					{
						call.arguments = arguments.clone();
					}
					events.push(Event::ToolCallDelta {
						content_index,
						delta: args_delta,
						partial: self.partial(),
					});
				}
			}
		}

		forward(tx, events).await;
		Ok(())
	}

	/// Mistral reports cache hits under any of several usage field spellings.
	fn cached_prompt_tokens(usage: &Value) -> u64 {
		[
			&usage["promptTokensDetails"]["cachedTokens"],
			&usage["prompt_tokens_details"]["cached_tokens"],
			&usage["promptTokenDetails"]["cachedTokens"],
			&usage["prompt_token_details"]["cached_tokens"],
			&usage["numCachedTokens"],
			&usage["num_cached_tokens"],
		]
		.iter()
		.find_map(|value| value.as_u64())
		.unwrap_or(0)
	}

	/// Post-loop tail: close the open block, then finish every tool call
	/// (one full parse; arguments_error reported on the call).
	fn finish(&mut self) {
		if let Some(event) = self.close_open_block() {
			let _ = event; // the TUI derives ends from deltas; state is already final
		}
		let keys: Vec<String> = self.tool_calls_by_key.keys().cloned().collect();
		for key in keys {
			let Some(open) = self.tool_calls_by_key.remove(&key) else {
				continue;
			};
			let (arguments, arguments_error) = open.args.finish();
			if let Some(AssistantContent::ToolCall(call)) =
				self.message.content.get_mut(open.content_index)
			{
				call.arguments = arguments;
				call.arguments_error = arguments_error;
			}
		}
	}
}

fn map_chat_stop_reason(reason: &str) -> (StopReason, Option<String>) {
	match reason {
		"stop" => (StopReason::Stop, None),
		"length" | "model_length" => (StopReason::Length, None),
		"tool_calls" => (StopReason::ToolUse, None),
		// Transient server failures use this spelling so retries can engage.
		"error" => (
			StopReason::Error,
			Some("Provider stopped with: error (server error)".to_string()),
		),
		other => (
			StopReason::Error,
			Some(format!("Provider stopped with: {other}")),
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{SystemMessage, Tool};

	fn test_model() -> Model {
		Model::new(
			"mistral-test",
			"mistral-test",
			"mistral-conversations",
			"mistral",
			"",
		)
	}

	#[test]
	fn request_body_wire_shape() {
		let model = test_model();
		let mut system = SystemMessage::prompt("be brief");
		system.tools_added = Some(vec![Tool::new(
			"ls",
			"list files",
			json!({"type": "object", "properties": {"path": {"type": "string"}}}),
		)]);
		let ctx = TranscriptContext {
			messages: vec![Message::System(system), Message::user("list the files")],
		};
		let options = StreamOptions {
			max_tokens: Some(512),
			session_id: Some("sess-9".into()),
			..Default::default()
		};
		let body = request_body(&model, &ctx, &options);
		assert_eq!(body["model"], "mistral-test");
		assert_eq!(body["stream"], true);
		assert_eq!(body["messages"][0]["role"], "system");
		assert_eq!(body["messages"][0]["content"], "be brief");
		// UserContent::Text serializes as a block array (P2 wire shape).
		assert_eq!(body["messages"][1]["content"][0]["type"], "text");
		assert_eq!(body["messages"][1]["content"][0]["text"], "list the files");
		assert_eq!(body["max_tokens"], 512);
		assert_eq!(body["prompt_cache_key"], "sess-9");
		let tool = &body["tools"][0];
		assert_eq!(tool["type"], "function");
		assert_eq!(tool["function"]["name"], "ls");
		assert_eq!(tool["function"]["strict"], false);
	}

	#[test]
	fn assistant_history_replays_thinking_and_tool_calls() {
		let model = test_model();
		let ctx = TranscriptContext {
			messages: vec![
				Message::user("go"),
				Message::Assistant(AssistantMessage {
					content: vec![
						AssistantContent::Thinking(ThinkingContent {
							thinking: "reasoned".into(),
							thinking_signature: None,
							redacted: None,
						}),
						AssistantContent::ToolCall(ToolCall {
							id: "EUc1llF0".into(),
							name: "ls".into(),
							arguments: json!({"path": "."}),
							thought_signature: None,
							namespace: None,
							arguments_error: None,
						}),
					],
					..AssistantMessage::error(&model, StopReason::Pending, "")
				}),
				Message::ToolResult(crate::types::ToolResultMessage::text_result(
					"EUc1llF0", "ls", "a.txt", false,
				)),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let messages = body["messages"].as_array().unwrap();
		assert_eq!(messages.len(), 3);
		assert_eq!(messages[1]["role"], "assistant");
		assert_eq!(messages[1]["content"][0]["type"], "thinking");
		assert_eq!(messages[1]["content"][0]["thinking"][0]["text"], "reasoned");
		assert_eq!(messages[1]["tool_calls"][0]["id"], "EUc1llF0");
		assert_eq!(
			messages[1]["tool_calls"][0]["function"]["arguments"],
			r#"{"path":"."}"#
		);
		assert_eq!(messages[2]["role"], "tool");
		assert_eq!(messages[2]["tool_call_id"], "EUc1llF0");
		assert_eq!(messages[2]["content"][0]["text"], "a.txt");
	}

	fn session() -> MistralSession {
		MistralSession::new(&test_model())
	}

	#[tokio::test]
	async fn stream_switches_text_thinking_and_index_keyed_tool_calls() {
		let mut session = session();
		// One chunk carrying thinking then text; tool call deltas split by index.
		session
			.process_payload(
				r#"{"data":{"id":"resp_1","choices":[{"delta":{"content":[{"type":"thinking","thinking":[{"text":"ponder"}]},{"type":"text","text":"answer"}]}}]}}"#,
				&fwd_channel(),
			)
			.await
			.unwrap();
		assert_eq!(session.message.content.len(), 2);
		assert!(matches!(session.open_block, Some(OpenBlock::Text { .. })));

		session
			.process_payload(
				r#"{"data":{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"callA","function":{"name":"read","arguments":"{\"p\":"}}]}}]}}"#,
				&fwd_channel(),
			)
			.await
			.unwrap();
		session
			.process_payload(
				r#"{"data":{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}}"#,
				&fwd_channel(),
			)
			.await
			.unwrap();
		session
			.process_payload(
				r#"{"data":{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":110,"completion_tokens":20,"total_tokens":130,"prompt_tokens_details":{"cached_tokens":10}}}}"#,
				&fwd_channel(),
			)
			.await
			.unwrap();

		assert_eq!(session.message.stop_reason, StopReason::ToolUse);
		assert_eq!(session.message.usage.input, 100, "cached subtracted");
		assert_eq!(session.message.usage.cache_read, 10);
		let AssistantContent::Thinking(thinking) = &session.message.content[0] else {
			panic!("thinking first");
		};
		assert_eq!(thinking.thinking, "ponder");
		let AssistantContent::Text(text) = &session.message.content[1] else {
			panic!("text second");
		};
		assert_eq!(text.text, "answer");
		let AssistantContent::ToolCall(call) = &session.message.content[2] else {
			panic!("tool call third");
		};
		assert_eq!(call.id, "callA");
		session.finish();
		let AssistantContent::ToolCall(call) = &session.message.content[2] else {
			panic!("tool call after finish");
		};
		assert_eq!(call.arguments, json!({"p": "x"}));
	}

	#[tokio::test]
	async fn stop_reason_mapping_covers_mistral_spellings() {
		assert_eq!(map_chat_stop_reason("stop").0, StopReason::Stop);
		assert_eq!(map_chat_stop_reason("length").0, StopReason::Length);
		assert_eq!(map_chat_stop_reason("model_length").0, StopReason::Length);
		assert_eq!(map_chat_stop_reason("tool_calls").0, StopReason::ToolUse);
		let (reason, message) = map_chat_stop_reason("error");
		assert_eq!(reason, StopReason::Error);
		assert!(message.unwrap().contains("server error"));
	}

	/// A detached sender: process_payload forwards events that the test does
	/// not consume.
	fn fwd_channel() -> Sender<Event> {
		let (tx, _rx) = tokio::sync::mpsc::channel::<Event>(64);
		tx
	}
}
