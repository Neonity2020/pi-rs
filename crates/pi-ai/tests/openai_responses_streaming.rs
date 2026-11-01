//! End-to-end streaming tests for the OpenAI Responses adapter against a
//! local mock HTTP+SSE server (raw `TcpListener`, chunked transfer
//! encoding). Exercises the incremental event loop ported from
//! `pi/packages/ai/src/api/openai-responses.ts`: reasoning/text/function-call
//! slots keyed by `output_index`, argument accumulation, usage/stop-reason
//! mapping, and the error paths (HTTP status, `response.failed`, disconnect
//! before a terminal event, abort).

use std::time::{Duration, Instant};

use futures::StreamExt;
use pi_ai::stream::{AssistantMessageEvent, StreamOptions};
use pi_ai::types::{AssistantContent, ToolCall};
use pi_ai::{Message, Model, StopReason, normalize_context};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn model_at(base_url: &str) -> Model {
	Model::new(
		"gpt-test",
		"gpt-test",
		"openai-responses",
		"openai",
		base_url,
	)
}

fn reasoning_model_at(base_url: &str) -> Model {
	let mut model = model_at(base_url);
	model.reasoning = true;
	model
}

fn context() -> pi_ai::TranscriptContext {
	normalize_context(Some("be brief"), &[], vec![Message::user("hi")])
}

fn options() -> StreamOptions {
	StreamOptions {
		api_key: Some("test-key".to_string()),
		..Default::default()
	}
}

fn stream_for(base_url: &str) -> pi_ai::AssistantMessageEventStream {
	(pi_ai::providers::openai_responses::stream_fn())(&model_at(base_url), &context(), options())
}

fn frame(payload: &str) -> String {
	format!("data: {payload}\n\n")
}

fn chunk_record(bytes: &[u8]) -> Vec<u8> {
	let mut record = format!("{:x}\r\n", bytes.len()).into_bytes();
	record.extend_from_slice(bytes);
	record.extend_from_slice(b"\r\n");
	record
}

fn sse_response(frames: &[String], terminate: bool) -> Vec<u8> {
	let mut response =
		b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
			.to_vec();
	for frame in frames {
		response.extend_from_slice(&chunk_record(frame.as_bytes()));
	}
	if terminate {
		response.extend_from_slice(b"0\r\n\r\n");
	}
	response
}

async fn read_request(socket: &mut TcpStream) -> Vec<u8> {
	let mut buffer = Vec::new();
	let mut chunk = [0u8; 4096];
	loop {
		let Some(head_end) = buffer
			.windows(4)
			.position(|window| window == b"\r\n\r\n")
			.map(|position| position + 4)
		else {
			let read = socket.read(&mut chunk).await.expect("read request");
			if read == 0 {
				return buffer;
			}
			buffer.extend_from_slice(&chunk[..read]);
			continue;
		};
		let head = String::from_utf8_lossy(&buffer[..head_end]).to_lowercase();
		let content_length = head
			.lines()
			.find_map(|line| line.strip_prefix("content-length:"))
			.and_then(|value| value.trim().parse::<usize>().ok())
			.unwrap_or(0);
		while buffer.len() < head_end + content_length {
			let read = socket.read(&mut chunk).await.expect("read body");
			if read == 0 {
				return buffer;
			}
			buffer.extend_from_slice(&chunk[..read]);
		}
		return buffer;
	}
}

/// Serves raw `responses` sequentially, one per connection, then closes.
async fn serve_raw(responses: Vec<Vec<u8>>) -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		for response in responses {
			let Ok((mut socket, _)) = listener.accept().await else {
				break;
			};
			read_request(&mut socket).await;
			if socket.write_all(&response).await.is_err() {
				break;
			}
			let _ = socket.shutdown().await;
		}
	});
	format!("http://{addr}")
}

/// Collects events with a hard timeout so a broken stream fails the test
/// instead of hanging it.
async fn collect(mut stream: pi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
	let deadline = Instant::now() + READ_TIMEOUT;
	let mut events = Vec::new();
	while let Some(event) = tokio::time::timeout_at(deadline.into(), stream.next())
		.await
		.expect("stream finished in time")
	{
		let done = matches!(
			event,
			AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
		);
		events.push(event);
		if done {
			break;
		}
	}
	events
}

#[tokio::test]
async fn streams_text_reasoning_and_tool_call_incrementally() {
	let frames = vec![
		frame(r#"{"type":"response.created","response":{"id":"resp_1"}}"#),
		frame(
			r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
		),
		frame(
			r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"pondering"}"#,
		),
		frame(
			r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"pondering"}],"encrypted_content":"enc-1"}}"#,
		),
		frame(
			r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
		),
		frame(r#"{"type":"response.output_text.delta","output_index":1,"delta":"hel"}"#),
		frame(r#"{"type":"response.output_text.delta","output_index":1,"delta":"lo"}"#),
		frame(
			r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","phase":"final_answer","content":[{"type":"output_text","text":"hello"}]}}"#,
		),
		frame(
			r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"read","arguments":""}}"#,
		),
		frame(
			r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"a\""}"#,
		),
		frame(
			r#"{"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"path\":\"a.txt\"}"}"#,
		),
		frame(
			r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"read","arguments":"{\"path\":\"a.txt\"}"}}"#,
		),
		frame(
			r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":110,"output_tokens":20,"total_tokens":130,"input_tokens_details":{"cached_tokens":10},"output_tokens_details":{"reasoning_tokens":8}}}}"#,
		),
	];
	let base = serve_raw(vec![sse_response(&frames, true)]).await;
	let events = collect(stream_for(&base)).await;

	let mut text_deltas = Vec::new();
	let mut thinking = String::new();
	let mut saw_start = false;
	let mut finished_call: Option<ToolCall> = None;
	let mut done = None;
	for event in &events {
		match event {
			AssistantMessageEvent::Start { .. } => saw_start = true,
			AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(delta),
			AssistantMessageEvent::TextDelta { delta, .. } => text_deltas.push(delta.clone()),
			AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
				finished_call = Some(tool_call.clone())
			}
			AssistantMessageEvent::Done { message, .. } => done = Some(message.clone()),
			_ => {}
		}
	}
	assert!(saw_start);
	assert_eq!(thinking, "pondering");
	assert_eq!(text_deltas.join(""), "hello", "deltas arrive incrementally");
	let call = finished_call.expect("tool call end");
	assert_eq!(call.id, "call_1|fc_1");
	assert_eq!(
		call.arguments,
		json!({"path": "a.txt"}),
		"done replaces deltas"
	);
	let message = done.expect("done event");
	assert_eq!(message.text(), "hello");
	assert_eq!(message.stop_reason, StopReason::ToolUse);
	assert_eq!(message.usage.input, 100, "cached tokens subtracted");
	assert_eq!(message.usage.cache_read, 10);
	assert_eq!(message.usage.total_tokens, 130);
	// The reasoning item's raw JSON replays as the thinking signature.
	let AssistantMessageEvent::ThinkingEnd { .. } = events[2] else {
		panic!("thinking end after start, got {:?}", events[2]);
	};
	let AssistantContent::Thinking(thinking) = &message.content[0] else {
		panic!("thinking block first");
	};
	assert!(
		thinking
			.thinking_signature
			.as_deref()
			.is_some_and(|signature| signature.contains("enc-1"))
	);
}

#[tokio::test]
async fn http_error_status_carries_the_provider_message() {
	let body = br#"{"error":{"message":"quota exceeded"}}"#;
	let response = format!(
		"HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
		body.len(),
		String::from_utf8_lossy(body)
	)
	.into_bytes();
	let base = serve_raw(vec![response]).await;
	let events = collect(stream_for(&base)).await;
	let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
		panic!("expected error event, got {events:?}");
	};
	assert!(
		error
			.error_message
			.as_deref()
			.is_some_and(|message| message.contains("quota exceeded"))
	);
}

#[tokio::test]
async fn response_failed_maps_to_an_error() {
	let frames = vec![
		frame(r#"{"type":"response.created","response":{"id":"resp_1"}}"#),
		frame(
			r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
		),
	];
	let base = serve_raw(vec![sse_response(&frames, true)]).await;
	let events = collect(stream_for(&base)).await;
	let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
		panic!("expected error event");
	};
	assert!(
		error
			.error_message
			.as_deref()
			.is_some_and(|message| message.contains("server_error: boom"))
	);
}

#[tokio::test]
async fn disconnect_before_terminal_event_fails() {
	let frames = vec![
		frame(r#"{"type":"response.created","response":{"id":"resp_1"}}"#),
		frame(r#"{"type":"response.output_text.delta","output_index":0,"delta":"partial"}"#),
		// Body closes cleanly but no terminal `response.completed` arrived.
	];
	let base = serve_raw(vec![sse_response(&frames, true)]).await;
	let events = collect(stream_for(&base)).await;
	let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
		panic!("expected error event");
	};
	assert!(
		error
			.error_message
			.as_deref()
			.is_some_and(|message| message.contains("terminal response event"))
	);
}

#[tokio::test]
async fn abort_cancels_the_stream() {
	// A server that never answers keeps the request in flight for the abort.
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		read_request(&mut socket).await;
		// Hold the connection open; the abort drops the stream instead.
		tokio::time::sleep(Duration::from_secs(30)).await;
		let _ = socket.shutdown().await;
	});

	let signal = tokio_util_cancellation();
	let options = StreamOptions {
		api_key: Some("test-key".into()),
		signal: signal.clone(),
		..Default::default()
	};
	let stream =
		(pi_ai::providers::openai_responses::stream_fn())(&model_at(&base), &context(), options);
	let mut stream = stream;
	let first = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
	assert!(first.is_err(), "no events before the abort");
	signal.cancel();
	let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
		.await
		.expect("abort surfaces promptly");
	assert!(matches!(
		event,
		Some(AssistantMessageEvent::Error {
			reason: StopReason::Aborted,
			..
		})
	));
}

fn tokio_util_cancellation() -> tokio_util::sync::CancellationToken {
	tokio_util::sync::CancellationToken::new()
}

#[tokio::test]
async fn request_carries_responses_wire_shape() {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let handle = tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		let request = read_request(&mut socket).await;
		let text = String::from_utf8_lossy(&request).to_string();
		let body = text
			.split_once("\r\n\r\n")
			.map(|(_, body)| body.to_string())
			.unwrap_or_default();
		let _ = socket
			.write_all(&sse_response(
				&[frame(
					r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
				)],
				true,
			))
			.await;
		let _ = socket.shutdown().await;
		body
	});
	let base = format!("http://{addr}");
	let model = reasoning_model_at(&base);
	let options = StreamOptions {
		api_key: Some("test-key".into()),
		reasoning: Some(pi_ai::ThinkingLevel::Medium),
		session_id: Some("session-abc".into()),
		max_tokens: Some(4),
		..Default::default()
	};
	let stream = (pi_ai::providers::openai_responses::stream_fn())(&model, &context(), options);
	let _ = collect(stream).await;
	let body: Value = serde_json::from_str(&handle.await.unwrap()).unwrap();
	assert_eq!(body["model"], "gpt-test");
	assert_eq!(body["stream"], true);
	assert_eq!(body["store"], false);
	assert_eq!(body["prompt_cache_key"], "session-abc");
	assert_eq!(
		body["max_output_tokens"], 16,
		"clamped to the Responses minimum"
	);
	assert_eq!(
		body["reasoning"],
		json!({"effort": "medium", "summary": "auto"})
	);
	assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
	assert_eq!(body["input"][0]["role"], "developer");
	assert_eq!(body["input"][0]["content"], "be brief");
}
