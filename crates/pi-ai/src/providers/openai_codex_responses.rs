//! OpenAI Codex (ChatGPT backend) Responses adapter — port of the SSE path of
//! `pi/packages/ai/src/api/openai-codex-responses.ts` (first slice).
//!
//! The wire is the OpenAI Responses protocol against the ChatGPT backend
//! (`{base}/codex/responses`): a ChatGPT OAuth access token authenticates via
//! `Authorization: Bearer` plus the `chatgpt-account-id` extracted from the
//! JWT payload claim (`https://api.openai.com/auth`), the system prompt rides
//! in top-level `instructions` instead of an input item, and codex-specific
//! stream events (`error`, `response.failed`, `response.done` with its
//! `end_turn` marker and non-standard statuses) are mapped onto standard
//! Responses events before the shared slot processor
//! ([`super::openai_responses::ResponsesSession`]) runs.
//!
//! Not modeled (declared deferrals): the WebSocket transport and its
//! session/continuation cache (SSE only, matching upstream's fallback path),
//! zstd request compression (the backend also accepts plain JSON), the retry
//! loop (upstream `DEFAULT_MAX_RETRIES` is 0 anyway) and its
//! `maxRetries`/`timeoutMs` options, grammar tools, service-tier pricing,
//! `onPayload`/`onResponse` hooks and identity headers beyond
//! `originator: "pi"`.

use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use super::openai_completions::SseDecoder;
use super::openai_responses::{
	ResponsesSession, clamp_prompt_cache_key, convert_transcript, effort_string, forward,
};
use crate::model::Model;
use crate::stream::{
	AssistantMessageEvent as Event, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
use crate::types::{AssistantMessage, StopReason, ThinkingLevel};

/// Events buffered for the consumer before the producer task backpressures.
const EVENT_CHANNEL_CAPACITY: usize = 64;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

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
				if !forward_codex_payloads(&mut session, payloads, tx).await? {
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
		if !forward_codex_payloads(&mut session, payloads, tx).await? {
			return Ok(());
		}
		if decoder.is_done() {
			break;
		}
	}

	// Same tail checks as the OpenAI Responses adapter: a terminal response
	// event must have arrived and every tool call must be closed out.
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

/// Parses and forwards one batch of SSE payloads through the codex event
/// mapping. Returns `Ok(false)` when the consumer dropped the stream.
async fn forward_codex_payloads(
	session: &mut ResponsesSession,
	payloads: Vec<String>,
	tx: &Sender<Event>,
) -> Result<bool, StreamFailure> {
	for payload in payloads {
		let events = process_codex_payload(session, &payload).map_err(|message| StreamFailure {
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

// ---------------------------------------------------------------------------
// HTTP request
// ---------------------------------------------------------------------------

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
	let account_id = extract_account_id(&key)?;
	let url = resolve_codex_url(&model.base_url);

	// Codex expects an originator on every request; it is not attribution
	// (upstream `identityHeaders`).
	let mut request = reqwest::Client::new()
		.post(url)
		.header("authorization", format!("Bearer {key}"))
		.header("chatgpt-account-id", account_id)
		.header("originator", "pi")
		.header("OpenAI-Beta", "responses=experimental")
		.header("accept", "text/event-stream");
	if let Some(session_id) = &options.session_id {
		request = request
			.header("session-id", session_id)
			.header("x-client-request-id", session_id);
	}
	let response = request
		.json(&codex_request_body(model, context, options))
		.send()
		.await
		.map_err(|error| format!("request failed: {error}"))?;
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

/// `{base}/codex/responses` with the upstream normalization branches.
fn resolve_codex_url(base_url: &str) -> String {
	let raw = if base_url.trim().is_empty() {
		DEFAULT_CODEX_BASE_URL
	} else {
		base_url
	};
	let normalized = raw.trim_end_matches('/');
	if normalized.ends_with("/codex/responses") {
		normalized.to_string()
	} else if normalized.ends_with("/codex") {
		format!("{normalized}/responses")
	} else {
		format!("{normalized}/codex/responses")
	}
}

/// `chatgpt_account_id` from the JWT payload claim; a malformed token fails
/// the request exactly like upstream (`Failed to extract accountId`).
fn extract_account_id(token: &str) -> Result<String, String> {
	let failure = || "Failed to extract accountId from token".to_string();
	let parts: Vec<&str> = token.split('.').collect();
	if parts.len() != 3 {
		return Err(failure());
	}
	let payload_bytes = decode_jwt_segment(parts[1]).ok_or_else(failure)?;
	let payload: Value = serde_json::from_slice(&payload_bytes).map_err(|_| failure())?;
	payload[JWT_CLAIM_PATH]["chatgpt_account_id"]
		.as_str()
		.map(str::to_string)
		.ok_or_else(failure)
}

/// JWT segments are base64url; try the padded and unpadded alphabets.
fn decode_jwt_segment(segment: &str) -> Option<Vec<u8>> {
	for engine in [
		&base64::engine::general_purpose::URL_SAFE_NO_PAD,
		&base64::engine::general_purpose::URL_SAFE,
	] {
		if let Ok(bytes) = engine.decode(segment) {
			return Some(bytes);
		}
	}
	None
}

// ---------------------------------------------------------------------------
// Request body (buildRequestBody)
// ---------------------------------------------------------------------------

fn codex_request_body(
	model: &Model,
	context: &TranscriptContext,
	options: &StreamOptions,
) -> Value {
	let parts = convert_transcript(model, context);
	let instructions = parts.system_parts.join("\n\n");
	let mut body = json!({
		"model": model.id,
		"store": false,
		"stream": true,
		"instructions": if instructions.is_empty() {
			"You are a helpful assistant."
		} else {
			instructions.as_str()
		},
		"input": parts.input,
		"text": {"verbosity": "low"},
		"include": ["reasoning.encrypted_content"],
		"tool_choice": "auto",
		"parallel_tool_calls": true,
	});
	if let Some(session_id) = &options.session_id {
		body["prompt_cache_key"] = json!(clamp_prompt_cache_key(session_id));
	}
	if let Some(temperature) = options.temperature {
		body["temperature"] = json!(temperature);
	}
	if !parts.tools.is_empty() {
		body["tools"] = json!(
			parts
				.tools
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
	// Reasoning: an explicit level maps through (no thinking level map
	// modeled); "off" and no request fall back to effort "none" for
	// reasoning models, matching the upstream else-branch.
	match options.reasoning {
		Some(ThinkingLevel::Off) | None => {
			if model.reasoning {
				body["reasoning"] = json!({"effort": "none"});
			}
		}
		Some(level) => {
			body["reasoning"] = json!({"effort": effort_string(level), "summary": "auto"});
		}
	}
	body
}

// ---------------------------------------------------------------------------
// Codex event mapping (mapCodexEvents)
// ---------------------------------------------------------------------------

/// Maps codex-specific stream events onto standard Responses events, then
/// hands the payload to the shared session processor.
fn process_codex_payload(
	session: &mut ResponsesSession,
	payload: &str,
) -> Result<Vec<Event>, String> {
	let event: Value = serde_json::from_str(payload)
		.map_err(|error| format!("Invalid Codex SSE JSON: {error}"))?;
	match event.get("type").and_then(Value::as_str) {
		Some("error") => {
			// extractCodexEventError: message/code at the top level or nested
			// under `error`.
			let nested = &event["error"];
			let message = event["message"]
				.as_str()
				.or_else(|| nested["message"].as_str())
				.map(str::to_string);
			let code = event["code"]
				.as_str()
				.or_else(|| nested["code"].as_str())
				.map(str::to_string);
			Err(format!(
				"Codex error: {}",
				message.or(code).unwrap_or_else(|| event.to_string())
			))
		}
		Some("response.failed") => Err(event["response"]["error"]["message"]
			.as_str()
			.unwrap_or("Codex response failed")
			.to_string()),
		Some("response.done" | "response.completed" | "response.incomplete") => {
			let mut mapped = event.clone();
			mapped["type"] = json!("response.completed");
			let response = &event["response"];
			if let Some(end_turn) = response["end_turn"].as_bool() {
				session.message.end_turn = Some(end_turn);
			}
			// Normalize the status into the known set; unknown values are
			// dropped so the shared processor sees no status at all.
			let status = response["status"].as_str();
			let known = matches!(
				status,
				Some(
					"completed" | "incomplete" | "failed" | "cancelled" | "queued" | "in_progress"
				)
			);
			if !known && let Some(object) = mapped["response"].as_object_mut() {
				object.remove("status");
			}
			session.process_payload(&mapped.to_string())
		}
		_ => session.process_payload(payload),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::types::{Message, SystemMessage, Tool};

	fn codex_model() -> Model {
		let mut model = Model::new(
			"gpt-5.2-codex",
			"GPT-5.2 Codex",
			"openai-codex-responses",
			"openai-codex",
			"",
		);
		model.reasoning = true;
		model.context_window = 400_000;
		model.max_tokens = 128_000;
		model
	}

	fn system_with_tools() -> SystemMessage {
		let mut system = SystemMessage::prompt("help");
		system.tools_added = Some(vec![Tool::new("ls", "list", json!({"type": "object"}))]);
		system
	}

	// -- request body -------------------------------------------------------

	#[test]
	fn request_body_carries_codex_specific_fields() {
		let model = codex_model();
		let ctx = TranscriptContext {
			messages: vec![Message::user("hi")],
		};
		let options = StreamOptions {
			session_id: Some("sess".into()),
			..Default::default()
		};
		let body = codex_request_body(&model, &ctx, &options);
		// The system prompt rides in instructions, not in input.
		assert_eq!(body["instructions"], "You are a helpful assistant.");
		assert_eq!(
			body["input"],
			json!([{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
		);
		assert_eq!(body["store"], false);
		assert_eq!(body["stream"], true);
		assert_eq!(body["text"], json!({"verbosity": "low"}));
		assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
		assert_eq!(body["tool_choice"], "auto");
		assert_eq!(body["parallel_tool_calls"], true);
		assert_eq!(body["prompt_cache_key"], "sess");
		// Reasoning model without an explicit level still gets effort none.
		assert_eq!(body["reasoning"], json!({"effort": "none"}));
	}

	#[test]
	fn request_body_maps_system_tools_and_reasoning_levels() {
		let model = codex_model();
		let ctx = TranscriptContext {
			messages: vec![Message::System(system_with_tools()), Message::user("list")],
		};
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::High),
			..Default::default()
		};
		let body = codex_request_body(&model, &ctx, &options);
		assert_eq!(body["instructions"], "help");
		assert_eq!(
			body["input"],
			json!([{"role": "user", "content": [{"type": "input_text", "text": "list"}]}])
		);
		assert_eq!(body["tools"][0]["name"], "ls");
		assert_eq!(
			body["reasoning"],
			json!({"effort": "high", "summary": "auto"})
		);

		// Explicit off collapses to effort none.
		let options = StreamOptions {
			reasoning: Some(ThinkingLevel::Off),
			..Default::default()
		};
		let body = codex_request_body(&model, &ctx, &options);
		assert_eq!(body["reasoning"], json!({"effort": "none"}));

		// Non-reasoning model sends no reasoning field at all.
		let mut plain = codex_model();
		plain.reasoning = false;
		let body = codex_request_body(&plain, &ctx, &StreamOptions::default());
		assert!(body["reasoning"].is_null());
	}

	#[test]
	fn prompt_cache_key_clamps_to_64_chars() {
		assert_eq!(clamp_prompt_cache_key("s"), "s");
		let long = "x".repeat(70);
		assert_eq!(clamp_prompt_cache_key(&long).chars().count(), 64);
	}

	// -- URL / auth ----------------------------------------------------------

	#[test]
	fn codex_url_normalization_matches_upstream() {
		assert_eq!(
			resolve_codex_url(""),
			"https://chatgpt.com/backend-api/codex/responses"
		);
		assert_eq!(
			resolve_codex_url("https://proxy.local:8443/api//"),
			"https://proxy.local:8443/api/codex/responses"
		);
		assert_eq!(
			resolve_codex_url("https://proxy.local/api/codex"),
			"https://proxy.local/api/codex/responses"
		);
		assert_eq!(
			resolve_codex_url("https://proxy.local/api/codex/responses"),
			"https://proxy.local/api/codex/responses"
		);
	}

	fn jwt_with_account(account_id: &str) -> String {
		let segment = |value: &Value| {
			base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
		};
		let header = segment(&json!({"alg": "HS256", "typ": "JWT"}));
		let payload = segment(&json!({JWT_CLAIM_PATH: {"chatgpt_account_id": account_id}}));
		format!("{header}.{payload}.c2ln")
	}

	#[test]
	fn account_id_extracts_from_jwt_claim() {
		assert_eq!(
			extract_account_id(&jwt_with_account("acc-123")).unwrap(),
			"acc-123"
		);
		// Two-segment and claim-less tokens fail with the upstream message.
		assert_eq!(
			extract_account_id("not-a-jwt").unwrap_err(),
			"Failed to extract accountId from token"
		);
		let no_claim = format!(
			"{}.{}.c2ln",
			base64::engine::general_purpose::URL_SAFE_NO_PAD
				.encode(json!({"alg": "HS256"}).to_string()),
			base64::engine::general_purpose::URL_SAFE_NO_PAD
				.encode(json!({"sub": "x"}).to_string()),
		);
		assert_eq!(
			extract_account_id(&no_claim).unwrap_err(),
			"Failed to extract accountId from token"
		);
	}

	// -- codex event mapping -------------------------------------------------

	fn session() -> ResponsesSession {
		ResponsesSession::new(&codex_model())
	}

	#[test]
	fn error_events_fail_with_codex_message() {
		let mut session = session();
		let error = process_codex_payload(
			&mut session,
			r#"{"type":"error","code":"limit","message":"rate limited"}"#,
		)
		.unwrap_err();
		assert_eq!(error, "Codex error: rate limited");

		let error = process_codex_payload(
			&mut session,
			r#"{"type":"error","error":{"code":"websocket_connection_limit_reached"}}"#,
		)
		.unwrap_err();
		assert_eq!(error, "Codex error: websocket_connection_limit_reached");
	}

	#[test]
	fn response_failed_maps_to_its_error_message() {
		let mut session = session();
		let error = process_codex_payload(
			&mut session,
			r#"{"type":"response.failed","response":{"error":{"code":"x","message":"bad request"}}}"#,
		)
		.unwrap_err();
		assert_eq!(error, "bad request");
		// Without a message the upstream fallback applies.
		let error =
			process_codex_payload(&mut session, r#"{"type":"response.failed","response":{}}"#)
				.unwrap_err();
		assert_eq!(error, "Codex response failed");
	}

	#[test]
	fn response_done_rewrites_to_completed_with_end_turn() {
		let mut session = session();
		let events = process_codex_payload(
			&mut session,
			r#"{"type":"response.done","response":{"id":"resp_1","status":"completed","end_turn":true,"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}}}"#,
		)
		.unwrap();
		assert!(events.is_empty());
		assert_eq!(session.message.end_turn, Some(true));
		assert_eq!(session.message.stop_reason, StopReason::Stop);
		assert_eq!(session.message.usage.input, 10);
		assert_eq!(session.message.usage.total_tokens, 14);
		// finish() sees the rewritten terminal event.
		assert!(session.finish().is_ok());
	}

	#[test]
	fn response_done_with_unknown_status_drops_it() {
		let mut session = session();
		process_codex_payload(
			&mut session,
			r#"{"type":"response.done","response":{"status":"bogus"}}"#,
		)
		.unwrap();
		// An absent status maps to the shared processor's stop-as-stop path.
		assert_eq!(session.message.stop_reason, StopReason::Stop);
		assert_eq!(session.message.raw_stop_reason.as_deref(), Some(""));
	}

	// -- mock HTTP round trip ------------------------------------------------

	#[tokio::test]
	async fn mock_http_round_trip_streams_events() {
		let response_body = concat!(
			"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
			"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n\n",
			"data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n",
			"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
			"data: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"end_turn\":true,\"usage\":{\"input_tokens\":12,\"output_tokens\":3,\"total_tokens\":15}}}\n\n",
			"data: [DONE]\n\n",
		);
		let response = format!(
			"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
			response_body.len(),
			response_body
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
				if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
					break;
				}
				if tokio::io::AsyncReadExt::read(&mut socket, &mut chunk)
					.await
					.unwrap_or(0) == 0
				{
					break;
				}
				buffer.extend_from_slice(&chunk);
			}
			let request = String::from_utf8_lossy(&buffer).to_string();
			assert!(
				request.starts_with("POST /codex/responses HTTP/1.1"),
				"request line: {request}"
			);
			assert!(
				request.contains("chatgpt-account-id: acc-123"),
				"account header: {request}"
			);
			assert!(
				request.contains("authorization: Bearer "),
				"bearer header: {request}"
			);
			assert!(
				request.contains("originator: pi"),
				"originator header: {request}"
			);
			assert!(
				request.contains("openai-beta: responses=experimental"),
				"beta header: {request}"
			);
			let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
			let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;
		});

		let model = Model::new(
			"gpt-5.2-codex",
			"GPT-5.2 Codex",
			"openai-codex-responses",
			"openai-codex",
			&base,
		);
		let ctx = TranscriptContext {
			messages: vec![Message::System(system_with_tools()), Message::user("hi")],
		};
		let options = StreamOptions {
			api_key: Some(jwt_with_account("acc-123")),
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
			let terminal = matches!(event, Event::Done { .. } | Event::Error { .. });
			events.push(event);
			if terminal {
				break;
			}
		}

		let Some(Event::Done { message, .. }) = events.last() else {
			panic!("done event, got {events:?}");
		};
		assert_eq!(message.response_id.as_deref(), Some("resp_1"));
		assert_eq!(message.stop_reason, StopReason::Stop);
		assert_eq!(message.end_turn, Some(true));
		assert_eq!(message.text(), "Hello");
		assert_eq!(message.usage.input, 12);
		assert_eq!(message.usage.total_tokens, 15);
	}
}
