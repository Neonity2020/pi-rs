//! pi-messages API adapter — port of `pi/packages/ai/src/api/pi-messages.ts`.
//!
//! The backend speaks pi's own protocol: one POST of
//! `{ model, context, options }` to `<baseUrl>/messages`, answered by an SSE
//! stream of already-serialized assistant-message events
//! (`text_start`/`text_delta`/… keyed by `contentIndex`) with a terminal
//! `done`/`error` carrying usage and the stop reason.
//!
//! Deferred: gateway rewrite diagnostics (the port's `AssistantMessage` has
//! no diagnostics sidecar), the `debug` flag, `toolChoice` (absent from the
//! port's StreamOptions — sent as omitted), and identity headers.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::openai_completions::SseDecoder;
use super::openai_responses::ToolArgsStream;
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{AssistantContent, AssistantMessage, StopReason, ToolCall, Usage, now_ms};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

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

/// A terminal stream failure; `partial` carries the accumulated assistant
/// message when the failure happened mid-stream.
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
	let mut session = PiMessagesSession::new(model);
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
	let mut failure: Option<Box<StreamFailure>> = None;
	loop {
		let chunk = match response.chunk().await {
			Ok(Some(chunk)) => chunk,
			Ok(None) => {
				let mut payloads = Vec::new();
				decoder.finish(&mut payloads);
				for payload in payloads {
					let terminal =
						session
							.process_payload(&payload, tx)
							.await
							.map_err(|message| StreamFailure {
								reason: StopReason::Error,
								message,
								partial: Some(session.partial()),
							})?;
					if terminal {
						// The terminal event already carried the final
						// message; nothing else touches the session.
						return Ok(());
					}
				}
				break;
			}
			Err(error) => {
				failure = Some(Box::new(StreamFailure {
					reason: StopReason::Error,
					message: format!("stream read failed: {error}"),
					partial: Some(session.message.clone()),
				}));
				break;
			}
		};
		let mut payloads = Vec::new();
		decoder.feed(&chunk, &mut payloads);
		for payload in payloads {
			let terminal = session
				.process_payload(&payload, tx)
				.await
				.map_err(|message| StreamFailure {
					reason: StopReason::Error,
					message,
					partial: Some(session.partial()),
				})?;
			if terminal {
				return Ok(());
			}
		}
		if decoder.is_done() {
			break;
		}
	}

	if let Some(failure) = failure {
		return Err(failure);
	}
	Err(Box::new(StreamFailure {
		reason: if options.signal.is_cancelled() {
			StopReason::Aborted
		} else {
			StopReason::Error
		},
		message: format!("{} stream ended without a terminal event", model.provider),
		partial: Some(session.into_message()),
	}))
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
		.filter(|s| !s.is_empty())
		.ok_or_else(|| format!("No API key provided for provider \"{}\"", model.provider))?;
	let base = model.base_url.trim_end_matches('/');
	let response = reqwest::Client::new()
		.post(format!("{base}/messages"))
		.bearer_auth(key)
		.header("accept", "text/event-stream")
		.json(&request_body(model, context, options))
		.send()
		.await
		.map_err(|e| format!("request failed: {e}"))?;
	let status = response.status();
	if !status.is_success() {
		let body = response.text().await.unwrap_or_default();
		// Upstream error shape: `{error: {message, code}}` with an HTTP
		// status line prefix.
		let parsed: Option<(Option<String>, Option<String>)> =
			serde_json::from_str::<Value>(&body).ok().and_then(|value| {
				let error = value.get("error")?;
				Some((
					error["message"].as_str().map(str::to_string),
					error["code"].as_str().map(str::to_string),
				))
			});
		let message = match parsed {
			Some((Some(message), code)) => format!(
				"{message}{}",
				code.map(|c| format!(" ({c})")).unwrap_or_default()
			),
			_ => {
				if body.trim().is_empty() {
					"request failed".to_string()
				} else {
					body.trim().to_string()
				}
			}
		};
		return Err(format!(
			"{status} {}: {message}",
			status.canonical_reason().unwrap_or("")
		));
	}
	Ok(response)
}

fn request_body(model: &Model, context: &TranscriptContext, options: &StreamOptions) -> Value {
	let mut options_body = serde_json::Map::new();
	if let Some(temperature) = options.temperature {
		options_body.insert("temperature".into(), json!(temperature));
	}
	if let Some(max_tokens) = options.max_tokens {
		options_body.insert("maxTokens".into(), json!(max_tokens));
	}
	if let Some(reasoning) = options.reasoning {
		options_body.insert("reasoning".into(), json!(reasoning));
	}
	if let Some(session_id) = &options.session_id {
		options_body.insert("sessionId".into(), json!(session_id));
	}
	json!({
		"model": model.id,
		"context": { "messages": context.messages },
		"options": Value::Object(options_body),
	})
}

/// Index-addressed content assembly over the backend's serialized events
/// (upstream `createEventConverter`): slots are written by `contentIndex`,
/// tool-call arguments stream through the tolerant partial parser, and the
/// terminal `done`/`error` completes the message with usage and ids.
struct PiMessagesSession {
	message: AssistantMessage,
	tool_arguments: HashMap<usize, ToolArgsStream>,
}

impl PiMessagesSession {
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
			tool_arguments: HashMap::new(),
		}
	}

	fn partial(&self) -> AssistantMessage {
		self.message.clone()
	}

	fn into_message(self) -> AssistantMessage {
		self.message
	}

	/// Grow the content list with empty text placeholders up to `index`
	/// (upstream indexes directly into the array).
	fn slot(&mut self, index: usize) -> &mut AssistantContent {
		while self.message.content.len() <= index {
			self.message
				.content
				.push(AssistantContent::Text(crate::types::TextContent {
					text: String::new(),
					text_signature: None,
				}));
		}
		&mut self.message.content[index]
	}
}

impl PiMessagesSession {
	async fn process_payload(&mut self, payload: &str, tx: &Sender<Event>) -> Result<bool, String> {
		let event: Value =
			serde_json::from_str(payload).map_err(|e| format!("malformed SSE payload: {e}"))?;
		let event_type = event
			.get("type")
			.and_then(Value::as_str)
			.ok_or_else(|| "SSE payload without a type".to_string())?
			.to_string();
		let content_index = event["contentIndex"].as_u64().unwrap_or(0) as usize;
		let mut events = Vec::new();
		match event_type.as_str() {
			"start" => {}
			"text_start" => {
				*self.slot(content_index) = AssistantContent::Text(crate::types::TextContent {
					text: String::new(),
					text_signature: None,
				});
				events.push(Event::TextStart {
					content_index,
					partial: self.partial(),
				});
			}
			"text_delta" => {
				let delta = event["delta"].as_str().unwrap_or_default().to_string();
				if let AssistantContent::Text(text) = self.slot(content_index) {
					text.text.push_str(&delta);
				}
				events.push(Event::TextDelta {
					content_index,
					delta,
					partial: self.partial(),
				});
			}
			"text_end" => {
				let signature = event["contentSignature"].as_str().map(str::to_string);
				let content = event["content"].as_str().unwrap_or_default().to_string();
				if let AssistantContent::Text(text) = self.slot(content_index) {
					text.text = content.clone();
					text.text_signature = signature;
				}
				events.push(Event::TextEnd {
					content_index,
					content,
					partial: self.partial(),
				});
			}
			"thinking_start" => {
				*self.slot(content_index) =
					AssistantContent::Thinking(crate::types::ThinkingContent {
						thinking: String::new(),
						thinking_signature: None,
						redacted: None,
					});
				events.push(Event::ThinkingStart {
					content_index,
					partial: self.partial(),
				});
			}
			"thinking_delta" => {
				let delta = event["delta"].as_str().unwrap_or_default().to_string();
				if let AssistantContent::Thinking(thinking) = self.slot(content_index) {
					thinking.thinking.push_str(&delta);
				}
				events.push(Event::ThinkingDelta {
					content_index,
					delta,
					partial: self.partial(),
				});
			}
			"thinking_end" => {
				let signature = event["contentSignature"].as_str().map(str::to_string);
				let content = event["content"].as_str().unwrap_or_default().to_string();
				let redacted = event["redacted"].as_bool();
				if let AssistantContent::Thinking(thinking) = self.slot(content_index) {
					thinking.thinking = content.clone();
					thinking.thinking_signature = signature;
					thinking.redacted = redacted;
				}
				events.push(Event::ThinkingEnd {
					content_index,
					content,
					partial: self.partial(),
				});
			}
			"toolcall_start" => {
				let id = event["id"].as_str().unwrap_or_default().to_string();
				let name = event["toolName"].as_str().unwrap_or_default().to_string();
				*self.slot(content_index) = AssistantContent::ToolCall(ToolCall {
					id,
					name,
					arguments: json!({}),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				});
				self.tool_arguments
					.insert(content_index, ToolArgsStream::new());
				events.push(Event::ToolCallStart {
					content_index,
					partial: self.partial(),
				});
			}
			"toolcall_delta" => {
				let delta = event["delta"].as_str().unwrap_or_default().to_string();
				let arguments = match self.tool_arguments.get_mut(&content_index) {
					Some(args) => args.append(&delta),
					None => json!({}),
				};
				if let AssistantContent::ToolCall(call) = self.slot(content_index) {
					call.arguments = arguments.clone();
				}
				events.push(Event::ToolCallDelta {
					content_index,
					delta,
					partial: self.partial(),
				});
			}
			"toolcall_end" => {
				let (arguments, arguments_error) = match self.tool_arguments.remove(&content_index)
				{
					Some(args) => args.finish(),
					None => (json!({}), None),
				};
				if let Some(call_value) = event.get("toolCall")
					&& let AssistantContent::ToolCall(call) = self.slot(content_index)
				{
					if let Some(id) = call_value["id"].as_str() {
						call.id = id.to_string();
					}
					if let Some(name) = call_value["name"].as_str() {
						call.name = name.to_string();
					}
					call.arguments = call_value["arguments"].clone();
					call.thought_signature =
						call_value["thoughtSignature"].as_str().map(str::to_string);
					call.namespace = call_value["namespace"].as_str().map(str::to_string);
					call.arguments_error =
						call_value["argumentsError"].as_str().map(str::to_string);
				}
				if let AssistantContent::ToolCall(call) = self.slot(content_index) {
					// The authoritative full parse still applies when the
					// backend echoes the arguments it streamed.
					if call.arguments.is_null() {
						call.arguments = arguments;
						call.arguments_error = arguments_error;
					}
				}
				let call = match self.message.content.get(content_index) {
					Some(AssistantContent::ToolCall(call)) => call.clone(),
					_ => return Err("toolcall_end without an open call".to_string()),
				};
				events.push(Event::ToolCallEnd {
					content_index,
					tool_call: call,
					partial: self.partial(),
				});
			}
			"done" | "error" => {
				let done = event_type == "done";
				self.message.stop_reason = if done {
					match event["reason"].as_str() {
						Some("length") => StopReason::Length,
						Some("toolUse") => StopReason::ToolUse,
						_ => StopReason::Stop,
					}
				} else if event["reason"].as_str() == Some("aborted") {
					StopReason::Aborted
				} else {
					StopReason::Error
				};
				if let Some(usage) = event.get("usage") {
					self.message.usage = serde_json::from_value(usage.clone()).unwrap_or_default();
				}
				self.message.response_id = event["responseId"].as_str().map(str::to_string);
				self.message.response_model = event["responseModel"].as_str().map(str::to_string);
				if !done {
					self.message.error_message = event["errorMessage"].as_str().map(str::to_string);
				}
				let final_message = self.message.clone();
				if done {
					events.push(Event::Done {
						reason: final_message.stop_reason,
						message: final_message,
					});
				} else {
					events.push(Event::Error {
						reason: final_message.stop_reason,
						error: final_message,
					});
				}
				// Forward the terminal event before reporting completion.
				forward(tx, events).await;
				return Ok(true);
			}
			other => return Err(format!("unknown pi-messages event type: {other}")),
		}
		if !forward(tx, events).await {
			// Consumer gone: stop quietly; the producer task exits.
			return Ok(true);
		}
		Ok(false)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::stream::{AssistantMessageEvent, normalize_context};
	use crate::types::{Message, ThinkingLevel};
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	fn test_model() -> Model {
		Model::new("pi-msg-test", "pi-msg-test", "pi-messages", "pi", "")
	}

	#[test]
	fn request_body_carries_context_and_options() {
		let model = test_model();
		let ctx = normalize_context(Some("be brief"), &[], vec![Message::user("hi")]);
		let options = StreamOptions {
			temperature: Some(0.5),
			max_tokens: Some(2048),
			reasoning: Some(ThinkingLevel::Low),
			session_id: Some("sess-1".into()),
			..Default::default()
		};
		let body = request_body(&model, &ctx, &options);
		assert_eq!(body["model"], "pi-msg-test");
		assert_eq!(body["context"]["messages"][0]["role"], "system");
		assert_eq!(body["context"]["messages"][0]["content"], "be brief");
		assert_eq!(body["context"]["messages"][1]["role"], "user");
		assert_eq!(body["options"]["temperature"], 0.5);
		assert_eq!(body["options"]["maxTokens"], 2048);
		assert_eq!(body["options"]["reasoning"], "low");
		assert_eq!(body["options"]["sessionId"], "sess-1");
	}

	#[tokio::test]
	async fn stream_maps_indexed_events_to_blocks() {
		let frames = vec![
			// contentIndex 1 starts first to prove slots are index-addressed.
			r#"data: {"type":"text_start","contentIndex":1}"#,
			r#"data: {"type":"text_delta","contentIndex":1,"delta":"hello"}"#,
			r#"data: {"type":"text_end","contentIndex":1,"content":"hello","contentSignature":"sig-1"}"#,
			r#"data: {"type":"toolcall_start","contentIndex":0,"id":"call_1","toolName":"read"}"#,
			r#"data: {"type":"toolcall_delta","contentIndex":0,"delta":"{\"path\":"}"#,
			r#"data: {"type":"toolcall_delta","contentIndex":0,"delta":"\"a\"}"}"#,
			r#"data: {"type":"toolcall_end","contentIndex":0,"toolCall":{"id":"call_1","name":"read","arguments":{"path":"a"}}}"#,
			r#"data: {"type":"done","reason":"toolUse","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"responseId":"resp_1","responseModel":"pi-msg-actual"}"#,
		];
		let mut body = String::new();
		for frame in frames {
			body.push_str(&format!("{:x}\r\n{}\n\n\r\n", frame.len() + 2, frame));
		}
		let response = format!(
			"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{body}0\r\n\r\n"
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		tokio::spawn(async move {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let mut buffer = Vec::new();
			let mut chunk = [0u8; 4096];
			loop {
				if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
					break;
				}
				if socket.read(&mut chunk).await.unwrap_or(0) == 0 {
					break;
				}
				buffer.extend_from_slice(&chunk);
			}
			let _ = socket.write_all(response.as_bytes()).await;
			let _ = socket.shutdown().await;
		});
		use tokio::io::AsyncWriteExt;

		let model = Model::new("pi-msg-test", "pi-msg-test", "pi-messages", "pi", &base);
		let ctx = normalize_context(None, &[], vec![Message::user("hi")]);
		let options = StreamOptions {
			api_key: Some("test-key".into()),
			..Default::default()
		};
		let mut stream = (stream_fn())(&model, &ctx, options);
		use futures::StreamExt;
		let mut events = Vec::new();
		while let Some(event) =
			tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
				.await
				.expect("in time")
		{
			let terminal = matches!(
				event,
				AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
			);
			events.push(event);
			if terminal {
				break;
			}
		}

		// Slot 0: the tool call; slot 1: the text block.
		let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
			panic!("done event, got {events:?}");
		};
		assert_eq!(message.response_id.as_deref(), Some("resp_1"));
		assert_eq!(message.response_model.as_deref(), Some("pi-msg-actual"));
		assert_eq!(message.stop_reason, StopReason::ToolUse);
		let AssistantContent::ToolCall(call) = &message.content[0] else {
			panic!("tool call at slot 0");
		};
		assert_eq!(call.id, "call_1");
		assert_eq!(call.arguments, json!({"path": "a"}));
		let AssistantContent::Text(text) = &message.content[1] else {
			panic!("text at slot 1");
		};
		assert_eq!(text.text, "hello");
		assert_eq!(text.text_signature.as_deref(), Some("sig-1"));
		assert_eq!(message.usage.input, 10);
		assert_eq!(message.usage.total_tokens, 15);
	}

	#[tokio::test]
	async fn stream_without_terminal_event_fails() {
		let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n";
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		tokio::spawn(async move {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let mut buffer = Vec::new();
			let mut chunk = [0u8; 4096];
			loop {
				if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
					break;
				}
				if socket.read(&mut chunk).await.unwrap_or(0) == 0 {
					break;
				}
				buffer.extend_from_slice(&chunk);
			}
			let _ = socket.write_all(response.as_bytes()).await;
			let _ = socket.shutdown().await;
		});
		let model = Model::new("pi-msg-test", "pi-msg-test", "pi-messages", "pi", &base);
		let ctx = normalize_context(None, &[], vec![Message::user("hi")]);
		let options = StreamOptions {
			api_key: Some("test-key".into()),
			..Default::default()
		};
		let mut stream = (stream_fn())(&model, &ctx, options);
		use futures::StreamExt;
		let mut events = Vec::new();
		while let Some(event) =
			tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
				.await
				.expect("in time")
		{
			let terminal = matches!(event, AssistantMessageEvent::Error { .. });
			events.push(event);
			if terminal {
				break;
			}
		}
		let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
			panic!("error event");
		};
		assert!(
			error
				.error_message
				.as_deref()
				.is_some_and(|message| message.contains("without a terminal event"))
		);
	}
}
