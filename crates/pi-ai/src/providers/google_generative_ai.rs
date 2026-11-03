//! Google Generative AI adapter — port of the streaming `stream()` in
//! `pi/packages/ai/src/api/google-generative-ai.ts` plus the shared
//! conversion helpers of `google-shared.ts` (message/tool conversion,
//! thought-signature retention, stop-reason mapping).
//!
//! First slice: the default surface (function tools via
//! `parametersJsonSchema` with `const`→`enum` rewriting, thought summaries,
//! thinking level/budget config, tool-call ids for models that require
//! them, merged `functionResponse` user turns). Deferred with upstream
//! defaults off: `toolChoice` (absent from the port's StreamOptions),
//! constrained-sampling VALIDATED mode, `useParameters` (Cloud Code Assist
//! with Claude), request retry policy (consistent with the other ported
//! adapters), and `thinkingLevelMap` (Xhigh/Max clamp to High).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::openai_completions::SseDecoder;
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{
	AssistantContent, AssistantMessage, ImageContent, Message, StopReason, SystemContent,
	TextContent, TextOrImage, ThinkingContent, ThinkingLevel, ToolCall, Usage, now_ms,
};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Monotonic counter for synthesized tool-call ids (upstream
/// `toolCallCounter`).
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

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
	let mut session = GoogleSession::new(model);
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

	// Close the open text/thinking block, then run the terminal checks
	// (upstream's post-loop block).
	session.close_open_block();
	if !session.message.stop_reason_is_terminal() {
		return Err(StreamFailure {
			reason: StopReason::Error,
			message: "Google stream ended without a finish reason".to_string(),
			session: Some(Box::new(session.into_message())),
		});
	}
	if matches!(
		session.message.stop_reason,
		StopReason::Aborted | StopReason::Error
	) {
		let message = session
			.message
			.raw_stop_reason
			.as_deref()
			.map(|reason| format!("Provider stopped with: {reason}"))
			.unwrap_or_else(|| "An unknown error occurred".to_string());
		return Err(StreamFailure {
			reason: StopReason::Error,
			message,
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
	session: &mut GoogleSession,
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
		.or_else(|| std::env::var("GEMINI_API_KEY").ok())
		.filter(|s| !s.is_empty())
		.ok_or_else(|| "GEMINI_API_KEY not set".to_string())?;
	// Upstream's baseUrl already includes the version path (apiVersion "").
	let base = if model.base_url.is_empty() {
		"https://generativelanguage.googleapis.com/v1beta"
	} else {
		model.base_url.trim_end_matches('/')
	};
	let url = format!("{base}/models/{}:streamGenerateContent?alt=sse", model.id);
	let response = reqwest::Client::new()
		.post(url)
		.header("x-goog-api-key", key)
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

// ---------------------------------------------------------------------------
// Request building (convertMessages / convertTools / thinking config)
// ---------------------------------------------------------------------------

/// Gemini has no mid-conversation system messages: the leading prompt goes to
/// `systemInstruction` (upstream `getInitialSystemMessage` +
/// `getSystemMessageText` over the collapsed transcript).
fn system_instruction(context: &TranscriptContext) -> Option<String> {
	context.messages.first().and_then(|message| match message {
		Message::System(system) => Some(match &system.content {
			SystemContent::Text(text) => text.clone(),
			SystemContent::Blocks(blocks) => blocks
				.iter()
				.map(|block| block.text.as_str())
				.collect::<Vec<_>>()
				.join("\n"),
		}),
		_ => None,
	})
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

/// Models that require explicit tool-call ids in function calls/responses
/// (upstream `requiresToolCallId`).
fn requires_tool_call_id(model_id: &str) -> bool {
	if model_id.starts_with("claude-") || model_id.starts_with("gpt-oss-") {
		return true;
	}
	get_gemini_major_version(model_id).is_some_and(|version| version >= 3)
}

fn get_gemini_major_version(model_id: &str) -> Option<u32> {
	let lower = model_id.to_lowercase();
	let rest = lower
		.strip_prefix("gemini-live-")
		.or_else(|| lower.strip_prefix("gemini-"))?;
	let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
	digits.parse().ok()
}

/// Gemini 3+ supports images nested inside `functionResponse.parts`.
fn supports_multimodal_function_response(model_id: &str) -> bool {
	match get_gemini_major_version(model_id) {
		Some(version) => version >= 3,
		None => true,
	}
}

/// Gemini 3+ models use the discrete `thinkingLevel` control instead of the
/// token-budget control (upstream `usesGoogleThinkingLevel`).
fn uses_thinking_level(model_id: &str) -> bool {
	let id = model_id.to_lowercase();
	let is_gemini_3_plus = id.starts_with("gemini-")
		&& get_gemini_major_version(model_id).is_some_and(|version| version >= 3);
	is_gemini_3_plus
		|| id == "gemini-flash-latest"
		|| id == "gemini-flash-lite-latest"
		|| id.starts_with("gemma-4")
		|| id.starts_with("gemma4")
}

fn thinking_level_wire(level: ThinkingLevel) -> Option<&'static str> {
	match level {
		ThinkingLevel::Minimal => Some("MINIMAL"),
		ThinkingLevel::Low => Some("LOW"),
		ThinkingLevel::Medium => Some("MEDIUM"),
		ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("HIGH"),
		ThinkingLevel::Off => None,
	}
}

/// Token budgets per model family (upstream `getGoogleBudget`; the port has
/// no per-model `thinkingBudgets` overrides).
fn thinking_budget(model_id: &str, level: ThinkingLevel) -> i64 {
	let level = thinking_level_wire(level)
		.unwrap_or("MEDIUM")
		.to_lowercase();
	let budgets: &[(&str, [i64; 4])] = &[
		("2.5-pro", [128, 2048, 8192, 32768]),
		("2.5-flash-lite", [512, 2048, 8192, 24576]),
		("2.5-flash", [128, 2048, 8192, 24576]),
	];
	for (needle, table) in budgets {
		if model_id.contains(needle) {
			return match level.as_str() {
				"minimal" => table[0],
				"low" => table[1],
				"medium" => table[2],
				_ => table[3],
			};
		}
	}
	-1 // dynamic
}

/// Gemini ignores `const` in `parametersJsonSchema`; rewrite string/number
/// `const` values to single-value `enum`s (upstream `rewriteConstAsEnum`,
/// respecting name-map/subschema keyword nesting).
fn rewrite_const_as_enum(schema: &Value, is_name_map: bool) -> Value {
	const NAME_MAPS: [&str; 6] = [
		"properties",
		"patternProperties",
		"$defs",
		"definitions",
		"dependentSchemas",
		"dependencies",
	];
	const SUBSCHEMAS: [&str; 15] = [
		"additionalProperties",
		"unevaluatedProperties",
		"propertyNames",
		"items",
		"prefixItems",
		"additionalItems",
		"unevaluatedItems",
		"contains",
		"allOf",
		"anyOf",
		"oneOf",
		"not",
		"if",
		"then",
		"else",
	];
	match schema {
		Value::Array(items) => Value::Array(
			items
				.iter()
				.map(|item| rewrite_const_as_enum(item, is_name_map))
				.collect(),
		),
		Value::Object(object) => {
			let mut result = serde_json::Map::new();
			for (key, value) in object {
				// The name-map flag applies to this level's keys only; each
				// subschema value is a schema again (upstream resets it).
				if is_name_map || SUBSCHEMAS.contains(&key.as_str()) {
					result.insert(key.clone(), rewrite_const_as_enum(value, false));
				} else if NAME_MAPS.contains(&key.as_str()) {
					result.insert(key.clone(), rewrite_const_as_enum(value, true));
				} else {
					result.insert(key.clone(), value.clone());
				}
			}
			let rewrite_const = !is_name_map
				&& result
					.get("const")
					.is_some_and(|value| value.is_string() || value.is_number())
				&& !result.contains_key("enum");
			if rewrite_const && let Some(const_value) = result.remove("const") {
				result.insert("enum".into(), json!([const_value]));
			}
			Value::Object(result)
		}
		other => other.clone(),
	}
}

fn user_parts(content: &crate::types::UserContent) -> Vec<Value> {
	match content {
		crate::types::UserContent::Text(text) => vec![json!({ "text": text })],
		crate::types::UserContent::Blocks(blocks) => blocks
			.iter()
			.map(|block| match block {
				TextOrImage::Text(text) => json!({ "text": text.text }),
				TextOrImage::Image(image) => inline_data(image),
			})
			.collect(),
	}
}

fn inline_data(image: &ImageContent) -> Value {
	json!({ "inlineData": { "mimeType": image.mime_type, "data": image.data } })
}

/// Messages → Gemini `Content[]` (upstream `convertMessages`): the initial
/// system message is excluded (it becomes `systemInstruction`); tool results
/// merge into a single user turn; thought signatures replay only for the
/// same provider+model.
fn convert_contents(model: &Model, context: &TranscriptContext) -> Vec<Value> {
	let mut contents: Vec<Value> = Vec::new();
	let normalize_id = |id: &str| -> String {
		if !requires_tool_call_id(&model.id) {
			return id.to_string();
		}
		let sanitized: String = id
			.chars()
			.map(|c| {
				if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
					c
				} else {
					'_'
				}
			})
			.collect();
		sanitized.chars().take(64).collect()
	};

	let is_same_provider_and_model = |message: &AssistantMessage| {
		message.provider == model.provider && message.model == model.id
	};

	for message in &context.messages {
		match message {
			Message::System(_) => {}
			Message::User(user) => {
				let parts = user_parts(&user.content);
				if !parts.is_empty() {
					contents.push(json!({ "role": "user", "parts": parts }));
				}
			}
			Message::Assistant(assistant) => {
				let same = is_same_provider_and_model(assistant);
				let mut parts: Vec<Value> = Vec::new();
				for block in &assistant.content {
					match block {
						AssistantContent::Text(text) => {
							let signature = resolve_thought_signature(same, &text.text_signature);
							// Empty text stays only when it carries a
							// signature (Gemini requires the echo).
							if (text.text.is_empty() || text.text.trim().is_empty())
								&& signature.is_none()
							{
								continue;
							}
							let mut part = json!({ "text": text.text });
							if let Some(signature) = signature {
								part["thoughtSignature"] = json!(signature);
							}
							parts.push(part);
						}
						AssistantContent::Thinking(thinking) => {
							if same {
								let signature =
									resolve_thought_signature(same, &thinking.thinking_signature);
								if (thinking.thinking.is_empty()
									|| thinking.thinking.trim().is_empty())
									&& signature.is_none()
								{
									continue;
								}
								let mut part =
									json!({ "thought": true, "text": thinking.thinking });
								if let Some(signature) = signature {
									part["thoughtSignature"] = json!(signature);
								}
								parts.push(part);
							} else {
								if thinking.thinking.trim().is_empty() {
									continue;
								}
								parts.push(json!({ "text": thinking.thinking }));
							}
						}
						AssistantContent::ToolCall(call) => {
							let signature =
								resolve_thought_signature(same, &call.thought_signature);
							let mut function_call = json!({
								"name": call.name,
								"args": call.arguments,
							});
							if requires_tool_call_id(&model.id) {
								function_call["id"] = json!(normalize_id(&call.id));
							}
							let mut part = json!({ "functionCall": function_call });
							if let Some(signature) = signature {
								part["thoughtSignature"] = json!(signature);
							}
							parts.push(part);
						}
					}
				}
				if !parts.is_empty() {
					contents.push(json!({ "role": "model", "parts": parts }));
				}
			}
			Message::ToolResult(result) => {
				let text = result
					.content
					.iter()
					.filter_map(|block| block.text_content())
					.collect::<Vec<_>>()
					.join("\n");
				let images: Vec<&ImageContent> = if model.supports_images() {
					result
						.content
						.iter()
						.filter_map(|block| match block {
							TextOrImage::Image(image) => Some(image),
							_ => None,
						})
						.collect()
				} else {
					Vec::new()
				};
				let has_text = !text.trim().is_empty();
				let response_value = if has_text {
					json!(text)
				} else if !images.is_empty() {
					json!("(see attached image)")
				} else {
					json!("")
				};
				let mut function_response = json!({
					"name": result.tool_name,
					"response": if result.is_error {
						json!({ "error": response_value })
					} else {
						json!({ "output": response_value })
					},
				});
				if !images.is_empty() && supports_multimodal_function_response(&model.id) {
					function_response["parts"] = json!(
						images
							.iter()
							.map(|image| inline_data(image))
							.collect::<Vec<_>>()
					);
				}
				if requires_tool_call_id(&model.id) {
					function_response["id"] = json!(normalize_id(&result.tool_call_id));
				}
				let part = json!({ "functionResponse": function_response });

				// All function responses merge into a single user turn.
				let merges = contents.last().is_some_and(|last| {
					last["role"] == "user"
						&& last["parts"].as_array().is_some_and(|parts| {
							parts.iter().any(|p| p.get("functionResponse").is_some())
						})
				});
				if merges
					&& let Some(last) = contents.last_mut()
					&& let Some(parts) = last["parts"].as_array_mut()
				{
					parts.push(part);
				} else {
					contents.push(json!({ "role": "user", "parts": [part] }));
				}

				// Gemini < 3: images ride in a separate user message.
				if !images.is_empty() && !supports_multimodal_function_response(&model.id) {
					let mut parts = vec![json!({ "text": "Tool result image:" })];
					parts.extend(images.iter().map(|image| inline_data(image)));
					contents.push(json!({ "role": "user", "parts": parts }));
				}
			}
		}
	}
	contents
}

/// Thought signatures must be base64 (upstream `isValidThoughtSignature`) and
/// only replay for the same provider+model.
fn resolve_thought_signature(
	same_provider_and_model: bool,
	signature: &Option<String>,
) -> Option<String> {
	let signature = signature.as_deref()?;
	if !same_provider_and_model {
		return None;
	}
	if signature.is_empty() || signature.len() % 4 != 0 {
		return None;
	}
	if !signature
		.chars()
		.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
	{
		return None;
	}
	Some(signature.to_string())
}

pub(crate) fn request_body(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
) -> Value {
	let mut body = json!({
		"model": model.id,
		"contents": convert_contents(model, context),
	});

	if let Some(system) = system_instruction(context)
		&& !system.is_empty()
	{
		body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
	}

	let tools = current_tools(context);
	if !tools.is_empty() {
		body["tools"] = json!([{
			"functionDeclarations": tools
				.iter()
				.map(|tool| json!({
					"name": tool.name,
					"description": tool.description,
					"parametersJsonSchema": rewrite_const_as_enum(&tool.parameters, false),
				}))
				.collect::<Vec<_>>(),
		}]);
	}

	let mut generation_config = serde_json::Map::new();
	if let Some(temperature) = options.temperature {
		generation_config.insert("temperature".into(), json!(temperature));
	}
	if let Some(max_tokens) = options.max_tokens {
		generation_config.insert("maxOutputTokens".into(), json!(max_tokens));
	}

	// Thinking config: an explicit level on a reasoning model enables thought
	// summaries with either the discrete level (Gemini 3+) or a token budget;
	// an absent level on a reasoning model disables thinking.
	if model.reasoning {
		let thinking_config = match options.reasoning {
			Some(ThinkingLevel::Off) | None => json!({ "thinkingBudget": 0 }),
			Some(level) => {
				if uses_thinking_level(&model.id) {
					json!({
						"includeThoughts": true,
						"thinkingLevel": thinking_level_wire(level).unwrap_or("HIGH"),
					})
				} else {
					json!({
						"includeThoughts": true,
						"thinkingBudget": thinking_budget(&model.id, level),
					})
				}
			}
		};
		generation_config.insert("thinkingConfig".into(), thinking_config);
	}

	if !generation_config.is_empty() {
		body["generationConfig"] = Value::Object(generation_config);
	}
	body
}

// ---------------------------------------------------------------------------
// Stream processing
// ---------------------------------------------------------------------------

/// `retainThoughtSignature`: keep the last non-empty signature; some
/// backends only send it on the first delta of a block.
fn retain_thought_signature(
	existing: &Option<String>,
	incoming: &Option<String>,
) -> Option<String> {
	match incoming {
		Some(signature) if !signature.is_empty() => Some(signature.clone()),
		_ => existing.clone(),
	}
}

enum OpenBlock {
	Text { content_index: usize },
	Thinking { content_index: usize },
}

pub(crate) struct GoogleSession {
	message: AssistantMessage,
	/// The currently open text/thinking block (upstream `currentBlock`).
	open_block: Option<OpenBlock>,
}

impl GoogleSession {
	pub(crate) fn new(model: &Model) -> Self {
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
		}
	}

	pub(crate) fn partial(&self) -> AssistantMessage {
		self.message.clone()
	}

	pub(crate) fn into_message(self) -> AssistantMessage {
		self.message
	}

	fn push_content(&mut self, content: AssistantContent) -> usize {
		self.message.content.push(content);
		self.message.content.len() - 1
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

	/// Close the open block with its final content (upstream's end events).
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

	pub(crate) async fn process_payload_streamed(
		&mut self,
		payload: &str,
		tx: &Sender<Event>,
	) -> Result<bool, String> {
		let events = self.process_payload(payload)?;
		let terminal = events
			.iter()
			.any(|event| matches!(event, Event::Done { .. } | Event::Error { .. }));
		super::google_vertex::forward(tx, events).await;
		Ok(terminal)
	}

	pub(crate) fn process_payload(&mut self, payload: &str) -> Result<Vec<Event>, String> {
		let chunk: Value =
			serde_json::from_str(payload).map_err(|e| format!("malformed SSE payload: {e}"))?;

		// First non-empty responseId wins (upstream `||=`).
		if self.message.response_id.is_none()
			&& let Some(id) = chunk["responseId"].as_str()
			&& !id.is_empty()
		{
			self.message.response_id = Some(id.to_string());
		}

		let mut events = Vec::new();
		if let Some(parts) = chunk["candidates"][0]["content"]["parts"].as_array() {
			for part in parts {
				if let Some(text) = part["text"].as_str() {
					let is_thinking = part["thought"] == json!(true);
					// A different block type (or none) opens a new block.
					let switch = self.open_block.as_ref().map(|open| match open {
						OpenBlock::Text { .. } => is_thinking,
						OpenBlock::Thinking { .. } => !is_thinking,
					}) != Some(false);
					if switch {
						if let Some(event) = self.close_open_block() {
							events.push(event);
						}
						events.push(if is_thinking {
							self.open_thinking()
						} else {
							self.open_text()
						});
					}
					let Some(
						OpenBlock::Text { content_index } | OpenBlock::Thinking { content_index },
					) = self.open_block
					else {
						continue;
					};
					match self.message.content.get_mut(content_index) {
						Some(AssistantContent::Thinking(thinking)) => {
							thinking.thinking.push_str(text);
							thinking.thinking_signature = retain_thought_signature(
								&thinking.thinking_signature,
								&part["thoughtSignature"].as_str().map(str::to_string),
							);
							events.push(Event::ThinkingDelta {
								content_index,
								delta: text.to_string(),
								partial: self.partial(),
							});
						}
						Some(AssistantContent::Text(text_block)) => {
							text_block.text.push_str(text);
							text_block.text_signature = retain_thought_signature(
								&text_block.text_signature,
								&part["thoughtSignature"].as_str().map(str::to_string),
							);
							events.push(Event::TextDelta {
								content_index,
								delta: text.to_string(),
								partial: self.partial(),
							});
						}
						_ => {}
					}
				}

				if let Some(function_call) = part.get("functionCall") {
					if let Some(event) = self.close_open_block() {
						events.push(event);
					}
					self.open_block = None;

					// Synthesize an id when missing or duplicated.
					let provided = function_call["id"].as_str().map(str::to_string);
					let name = function_call["name"]
						.as_str()
						.unwrap_or_default()
						.to_string();
					let duplicate = provided.as_ref().is_some_and(|id| {
						self.message.content.iter().any(|block| match block {
							AssistantContent::ToolCall(call) => &call.id == id,
							_ => false,
						})
					});
					let tool_call_id = match provided.filter(|id| !id.is_empty() && !duplicate) {
						Some(id) => id,
						None => format!(
							"{name}_{}_{}",
							now_ms(),
							TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
						),
					};
					let call = ToolCall {
						id: tool_call_id,
						name: name.clone(),
						arguments: function_call
							.get("args")
							.cloned()
							.unwrap_or_else(|| json!({})),
						thought_signature: part["thoughtSignature"].as_str().map(str::to_string),
						namespace: None,
						arguments_error: None,
					};
					let content_index = self.push_content(AssistantContent::ToolCall(call.clone()));
					events.push(Event::ToolCallStart {
						content_index,
						partial: self.partial(),
					});
					events.push(Event::ToolCallDelta {
						content_index,
						delta: call.arguments.to_string(),
						partial: self.partial(),
					});
					events.push(Event::ToolCallEnd {
						content_index,
						tool_call: call,
						partial: self.partial(),
					});
				}
			}
		}

		if let Some(finish_reason) = chunk["candidates"][0]["finishReason"].as_str() {
			self.message.raw_stop_reason = Some(finish_reason.to_string());
			self.message.stop_reason = map_finish_reason(finish_reason);
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

		if let Some(usage) = chunk.get("usageMetadata") {
			let cached = usage["cachedContentTokenCount"].as_u64().unwrap_or(0);
			let thoughts = usage["thoughtsTokenCount"].as_u64().unwrap_or(0);
			self.message.usage = Usage {
				input: usage["promptTokenCount"]
					.as_u64()
					.unwrap_or(0)
					.saturating_sub(cached),
				output: usage["candidatesTokenCount"].as_u64().unwrap_or(0) + thoughts,
				cache_read: cached,
				cache_write: 0,
				cache_write_1h: None,
				reasoning: Some(thoughts).filter(|tokens| *tokens > 0),
				total_tokens: usage["totalTokenCount"].as_u64().unwrap_or(0),
				cost: Default::default(),
			};
		}

		Ok(events)
	}
}

impl AssistantMessage {
	fn stop_reason_is_terminal(&self) -> bool {
		!matches!(self.stop_reason, StopReason::Pending)
	}
}

/// Gemini finish reasons → stop reasons (upstream `mapStopReason`): STOP →
/// stop, MAX_TOKENS → length, every safety/other reason → error.
fn map_finish_reason(reason: &str) -> StopReason {
	match reason {
		"STOP" => StopReason::Stop,
		"MAX_TOKENS" => StopReason::Length,
		// STOP-adjacent and everything else the API can send lands on error
		// (BLOCKLIST, SAFETY, RECITATION, MALFORMED_FUNCTION_CALL, ...).
		_ => StopReason::Error,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::stream::normalize_context;
	use crate::types::{SystemMessage, Tool, ToolResultMessage, UserContent, UserMessage};

	fn test_model() -> Model {
		Model::new(
			"gemini-test",
			"gemini-test",
			"google-generative-ai",
			"google",
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
			json!({"type": "object", "properties": {"path": {"type": "string", "const": "."}}, "required": ["path"]}),
		)]);
		let ctx = TranscriptContext {
			messages: vec![Message::System(system), Message::user("list the files")],
		};
		let options = StreamOptions {
			max_tokens: Some(1024),
			temperature: Some(0.7),
			..Default::default()
		};
		let body = request_body(&model, &ctx, &options);
		assert_eq!(body["model"], "gemini-test");
		assert_eq!(
			body["systemInstruction"]["parts"][0]["text"], "be brief",
			"leading system message becomes systemInstruction"
		);
		assert_eq!(body["contents"][0]["role"], "user");
		assert_eq!(body["contents"][0]["parts"][0]["text"], "list the files");
		assert_eq!(body["generationConfig"]["temperature"], 0.7);
		assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
		// `const` rewrites to a single-value enum for parametersJsonSchema.
		assert_eq!(
			body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"]["path"]
				["enum"],
			json!(["."])
		);
		assert!(
			body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"]["path"]
				.get("const")
				.is_none()
		);
		assert!(
			body.get("toolConfig").is_none(),
			"no constrained mode without strict tools"
		);
	}

	#[test]
	fn thinking_config_follows_level_and_model_family() {
		let mut model = test_model();
		model.reasoning = true;
		let ctx = normalize_context(None, &[], vec![Message::user("hi")]);
		// Gemini 3+ uses the discrete level control.
		model.id = "gemini-3-pro-preview".into();
		let body = request_body(
			&model,
			&ctx,
			&StreamOptions {
				reasoning: Some(ThinkingLevel::Medium),
				..Default::default()
			},
		);
		assert_eq!(
			body["generationConfig"]["thinkingConfig"],
			json!({"includeThoughts": true, "thinkingLevel": "MEDIUM"})
		);
		// 2.5-flash uses the budget tables.
		model.id = "gemini-2.5-flash".into();
		let body = request_body(
			&model,
			&ctx,
			&StreamOptions {
				reasoning: Some(ThinkingLevel::Low),
				..Default::default()
			},
		);
		assert_eq!(
			body["generationConfig"]["thinkingConfig"],
			json!({"includeThoughts": true, "thinkingBudget": 2048})
		);
		// Off disables thinking.
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(
			body["generationConfig"]["thinkingConfig"],
			json!({"thinkingBudget": 0})
		);
		// Non-reasoning models send no thinkingConfig at all.
		model.reasoning = false;
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert!(body["generationConfig"].get("thinkingConfig").is_none());
	}

	#[test]
	fn tool_results_merge_into_one_user_turn() {
		let model = test_model();
		let ctx = TranscriptContext {
			messages: vec![
				Message::user("go"),
				Message::Assistant(AssistantMessage {
					content: vec![
						AssistantContent::ToolCall(ToolCall {
							id: "call_1".into(),
							name: "ls".into(),
							arguments: json!({}),
							thought_signature: None,
							namespace: None,
							arguments_error: None,
						}),
						AssistantContent::ToolCall(ToolCall {
							id: "call_2".into(),
							name: "cat".into(),
							arguments: json!({}),
							thought_signature: None,
							namespace: None,
							arguments_error: None,
						}),
					],
					..AssistantMessage::error(&test_model(), StopReason::Pending, "")
				}),
				Message::ToolResult(ToolResultMessage::text_result("call_1", "ls", "a", false)),
				Message::ToolResult(ToolResultMessage::text_result("call_2", "cat", "b", false)),
			],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let contents = body["contents"].as_array().unwrap();
		assert_eq!(contents.len(), 3, "user + model + merged user turn");
		assert_eq!(contents[2]["role"], "user");
		let parts = contents[2]["parts"].as_array().unwrap();
		assert_eq!(parts.len(), 2, "both function responses in one turn");
		assert_eq!(parts[0]["functionResponse"]["response"]["output"], "a");
		assert_eq!(parts[1]["functionResponse"]["response"]["output"], "b");
	}

	#[test]
	fn cross_provider_thinking_becomes_plain_text() {
		let mut model = test_model();
		model.provider = "google".into();
		let mut other = test_model();
		other.id = "other-model".into();
		let ctx = TranscriptContext {
			messages: vec![Message::Assistant(AssistantMessage {
				content: vec![
					AssistantContent::Thinking(ThinkingContent {
						thinking: "secret reasoning".into(),
						thinking_signature: Some("sig==".into()),
						redacted: None,
					}),
					AssistantContent::Text(TextContent {
						text: "answer".into(),
						text_signature: None,
					}),
				],
				provider: "openai".into(),
				model: "other-model".into(),
				..AssistantMessage::error(&other, StopReason::Pending, "")
			})],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let parts = body["contents"][0]["parts"].as_array().unwrap();
		assert!(
			parts[0].get("thought").is_none(),
			"cross-provider thinking is plain text"
		);
		assert_eq!(parts[0]["text"], "secret reasoning");
		assert!(parts[0].get("thoughtSignature").is_none());
	}

	#[test]
	fn same_model_replays_thought_signatures() {
		let model = test_model();
		let ctx = TranscriptContext {
			messages: vec![Message::Assistant(AssistantMessage {
				content: vec![AssistantContent::Thinking(ThinkingContent {
					thinking: "reasoned".into(),
					thinking_signature: Some("c2ln".into()),
					redacted: None,
				})],
				provider: model.provider.clone(),
				model: model.id.clone(),
				..AssistantMessage::error(&model, StopReason::Pending, "")
			})],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let part = &body["contents"][0]["parts"][0];
		assert_eq!(part["thought"], true);
		assert_eq!(part["thoughtSignature"], "c2ln");
	}

	#[test]
	fn gemini3_tool_calls_carry_ids() {
		let mut model = test_model();
		model.id = "gemini-3-flash-preview".into();
		let ctx = TranscriptContext {
			messages: vec![Message::Assistant(AssistantMessage {
				content: vec![AssistantContent::ToolCall(ToolCall {
					id: "call_1".into(),
					name: "ls".into(),
					arguments: json!({}),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				})],
				provider: model.provider.clone(),
				model: model.id.clone(),
				..AssistantMessage::error(&model, StopReason::Pending, "")
			})],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert_eq!(
			body["contents"][0]["parts"][0]["functionCall"]["id"],
			"call_1"
		);
		// Gemini 2 does not carry ids.
		model.id = "gemini-2.5-flash".into();
		let body = request_body(&model, &ctx, &StreamOptions::default());
		assert!(
			body["contents"][0]["parts"][0]["functionCall"]
				.get("id")
				.is_none()
		);
	}

	fn session() -> GoogleSession {
		GoogleSession::new(&test_model())
	}

	#[test]
	fn stream_switches_between_thinking_and_text_blocks() {
		let mut session = session();
		session
			.process_payload(
				r#"{"responseId":"r1","candidates":[{"content":{"parts":[{"thought":true,"text":"hmm"},{"text":"answer"}]}}]}"#,
			)
			.unwrap();
		assert_eq!(session.message.content.len(), 2);
		assert!(matches!(session.open_block, Some(OpenBlock::Text { .. })));
		let close = session.close_open_block().expect("closes text");
		match close {
			Event::TextEnd { content, .. } => assert_eq!(content, "answer"),
			other => panic!("text end expected, got {other:?}"),
		}
		let AssistantContent::Thinking(thinking) = &session.message.content[0] else {
			panic!("thinking first");
		};
		assert_eq!(thinking.thinking, "hmm");
		assert_eq!(session.message.response_id.as_deref(), Some("r1"));
	}

	#[test]
	fn synthesized_tool_call_ids_are_unique_per_call() {
		let mut session = session();
		session
			.process_payload(
				r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"ls","args":{"path":"."}}}]},"finishReason":"STOP"}]}"#,
			)
			.unwrap();
		let first = match &session.message.content[0] {
			AssistantContent::ToolCall(call) => call.clone(),
			_ => panic!("tool call"),
		};
		assert!(first.id.starts_with("ls_"), "synthesized id: {}", first.id);
		assert_eq!(session.message.stop_reason, StopReason::ToolUse, "upgraded");
		// A duplicate provided id gets a fresh synthesized one.
		session
			.process_payload(
				r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"dup","name":"ls","args":{}}}]}}]}"#,
			)
			.unwrap();
		session
			.process_payload(
				r#"{"candidates":[{"content":{"parts":[{"functionCall":{"id":"dup","name":"ls","args":{}}}]}}]}"#,
			)
			.unwrap();
		let ids: Vec<String> = session
			.message
			.content
			.iter()
			.filter_map(|block| match block {
				AssistantContent::ToolCall(call) => Some(call.id.clone()),
				_ => None,
			})
			.collect();
		assert_ne!(ids[1], ids[2], "duplicate provided id regenerated");
	}

	#[test]
	fn usage_subtracts_cached_and_counts_thoughts() {
		let mut session = session();
		session
			.process_payload(
				r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":120,"candidatesTokenCount":30,"thoughtsTokenCount":10,"cachedContentTokenCount":20,"totalTokenCount":160}}"#,
			)
			.unwrap();
		assert_eq!(session.message.usage.input, 100, "cached tokens subtracted");
		assert_eq!(
			session.message.usage.output, 40,
			"thoughts counted in output"
		);
		assert_eq!(session.message.usage.cache_read, 20);
		assert_eq!(session.message.usage.reasoning, Some(10));
		assert_eq!(session.message.usage.total_tokens, 160);
	}

	#[test]
	fn empty_text_without_signature_is_dropped_on_replay() {
		let model = test_model();
		let ctx = TranscriptContext {
			messages: vec![Message::Assistant(AssistantMessage {
				content: vec![
					AssistantContent::Text(TextContent {
						text: String::new(),
						text_signature: None,
					}),
					AssistantContent::Text(TextContent {
						text: String::new(),
						text_signature: Some("c2ln".into()),
					}),
				],
				provider: model.provider.clone(),
				model: model.id.clone(),
				..AssistantMessage::error(&model, StopReason::Pending, "")
			})],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let parts = body["contents"][0]["parts"].as_array().unwrap();
		assert_eq!(
			parts.len(),
			1,
			"only the signature-bearing empty text stays"
		);
		assert!(parts[0].get("thoughtSignature").is_some());
	}

	#[test]
	fn user_images_become_inline_data() {
		let mut model = test_model();
		model.input = vec![
			crate::model::InputModality::Text,
			crate::model::InputModality::Image,
		];
		let ctx = TranscriptContext {
			messages: vec![Message::User(UserMessage {
				content: UserContent::Blocks(vec![
					TextOrImage::text("look"),
					TextOrImage::Image(ImageContent {
						data: "aGk=".into(),
						mime_type: "image/png".into(),
					}),
				]),
				timestamp: 0,
			})],
		};
		let body = request_body(&model, &ctx, &StreamOptions::default());
		let parts = body["contents"][0]["parts"].as_array().unwrap();
		assert_eq!(parts[0]["text"], "look");
		assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
	}
}
