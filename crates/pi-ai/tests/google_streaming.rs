//! End-to-end streaming tests for the Google Generative AI adapter against a
//! local mock HTTP+SSE server (raw `TcpListener`, chunked transfer
//! encoding). Exercises the incremental event loop ported from
//! `pi/packages/ai/src/api/google-generative-ai.ts`: block switching on the
//! `thought` flag, tool-call id synthesis, usage accounting, and the error
//! paths (HTTP status, stream without a finish reason, abort).

use std::time::{Duration, Instant};

use futures::StreamExt;
use pi_ai::stream::{AssistantMessageEvent, StreamOptions};
use pi_ai::{Message, Model, StopReason, normalize_context};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn model_at(base_url: &str) -> Model {
	Model::new(
		"gemini-test",
		"gemini-test",
		"google-generative-ai",
		"google",
		base_url,
	)
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
	(pi_ai::providers::google_generative_ai::stream_fn())(
		&model_at(base_url),
		&context(),
		options(),
	)
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
async fn streams_thinking_text_and_tool_call_across_chunks() {
	let frames = vec![
		frame(
			r#"{"responseId":"r1","candidates":[{"content":{"parts":[{"thought":true,"text":"thinking"}]}}]}"#,
		),
		frame(r#"{"candidates":[{"content":{"parts":[{"text":"partial "}]}}]}"#),
		frame(
			r#"{"candidates":[{"content":{"parts":[{"text":"answer"},{"functionCall":{"name":"read","args":{"path":"a"}}}]}}]}"#,
		),
		frame(
			r#"{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":110,"candidatesTokenCount":20,"thoughtsTokenCount":5,"cachedContentTokenCount":10,"totalTokenCount":145}}"#,
		),
	];
	let base = serve_raw(vec![sse_response(&frames, true)]).await;
	let events = collect(stream_for(&base)).await;

	let mut thinking = String::new();
	let mut text = String::new();
	let mut finished_call: Option<pi_ai::ToolCall> = None;
	let mut block_switches = 0;
	for event in &events {
		match event {
			AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(delta),
			AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
			AssistantMessageEvent::TextStart { .. }
			| AssistantMessageEvent::ThinkingStart { .. } => {
				block_switches += 1;
			}
			AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
				finished_call = Some(tool_call.clone())
			}
			_ => {}
		}
	}
	assert_eq!(block_switches, 2, "one thinking block, one text block");
	assert_eq!(thinking, "thinking");
	assert_eq!(text, "partial answer", "text spans multiple chunks");
	let call = finished_call.expect("tool call end");
	assert!(call.id.starts_with("read_"), "synthesized id: {}", call.id);
	assert_eq!(call.arguments, json!({"path": "a"}));
	let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
		panic!("done event");
	};
	assert_eq!(message.stop_reason, StopReason::ToolUse, "upgraded");
	assert_eq!(message.usage.input, 100, "cached tokens subtracted");
	assert_eq!(message.usage.output, 25, "thoughts counted");
	assert_eq!(message.usage.total_tokens, 145);
	// The first delta carries the block switch; the thinking block precedes
	// the text block in the content.
	let AssistantContent::Thinking(first) = &message.content[0] else {
		panic!("thinking first");
	};
	assert_eq!(first.thinking, "thinking");
}

use pi_ai::types::AssistantContent;

#[tokio::test]
async fn http_error_carries_the_provider_message() {
	let body = br#"{"error":{"code":429,"message":"resource exhausted"}}"#;
	let response = format!(
		"HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
		body.len(),
		String::from_utf8_lossy(body)
	)
	.into_bytes();
	let base = serve_raw(vec![response]).await;
	let events = collect(stream_for(&base)).await;
	let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
		panic!("expected error event");
	};
	assert!(
		error
			.error_message
			.as_deref()
			.is_some_and(|message| message.contains("resource exhausted"))
	);
}

#[tokio::test]
async fn stream_without_finish_reason_fails() {
	let frames = vec![frame(
		r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
	)];
	let base = serve_raw(vec![sse_response(&frames, true)]).await;
	let events = collect(stream_for(&base)).await;
	let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
		panic!("expected error event");
	};
	assert!(
		error
			.error_message
			.as_deref()
			.is_some_and(|message| message.contains("without a finish reason"))
	);
}

#[tokio::test]
async fn safety_stop_maps_to_error() {
	let frames = vec![frame(
		r#"{"candidates":[{"finishReason":"SAFETY","content":{"parts":[{"text":"no"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#,
	)];
	let base = serve_raw(vec![sse_response(&frames, true)]).await;
	let events = collect(stream_for(&base)).await;
	let Some(AssistantMessageEvent::Error { error, .. }) = events.last() else {
		panic!("expected error event");
	};
	assert!(
		error
			.error_message
			.as_deref()
			.is_some_and(|message| message.contains("Provider stopped with: SAFETY"))
	);
}

#[tokio::test]
async fn abort_cancels_the_stream() {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let base = format!("http://{}", listener.local_addr().unwrap());
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		read_request(&mut socket).await;
		tokio::time::sleep(Duration::from_secs(30)).await;
		let _ = socket.shutdown().await;
	});
	let signal = tokio_util::sync::CancellationToken::new();
	let options = StreamOptions {
		api_key: Some("test-key".into()),
		signal: signal.clone(),
		..Default::default()
	};
	let mut stream = (pi_ai::providers::google_generative_ai::stream_fn())(
		&model_at(&base),
		&context(),
		options,
	);
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
