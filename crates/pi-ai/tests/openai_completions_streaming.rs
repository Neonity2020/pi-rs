//! End-to-end streaming tests for the OpenAI-compatible adapter against a
//! local mock HTTP+SSE server (raw `TcpListener`, chunked transfer encoding).
//! Exercises the incremental chunk loop ported from
//! `pi/packages/ai/src/api/openai-completions.ts`.

use std::time::{Duration, Instant};

use futures::StreamExt;
use pi_ai::stream::{AssistantMessageEvent, StreamOptions};
use pi_ai::{Message, Model, StopReason, normalize_context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn model_at(base_url: &str) -> Model {
	Model::new(
		"test-model",
		"test",
		"openai-completions",
		"openai",
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
	(pi_ai::providers::openai_completions::stream_fn())(&model_at(base_url), &context(), options())
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

/// Reads one HTTP request (head plus content-length body) from the socket.
async fn drain_request(socket: &mut TcpStream) {
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
				return;
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
				return;
			}
			buffer.extend_from_slice(&chunk[..read]);
		}
		return;
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
			drain_request(&mut socket).await;
			if socket.write_all(&response).await.is_err() {
				break;
			}
			let _ = socket.shutdown().await;
		}
	});
	format!("http://{addr}")
}

/// Streams `frames` as individual chunked records with `gap` between them so
/// the client observes true incremental delivery, then terminates the body.
async fn serve_sse_slow(frames: Vec<String>, gap: Duration) -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		drain_request(&mut socket).await;
		let head =
			b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
		if socket.write_all(head).await.is_err() {
			return;
		}
		for frame in frames {
			if socket
				.write_all(&chunk_record(frame.as_bytes()))
				.await
				.is_err()
			{
				return;
			}
			tokio::time::sleep(gap).await;
		}
		let _ = socket.write_all(b"0\r\n\r\n").await;
		let _ = socket.shutdown().await;
	});
	format!("http://{addr}")
}

async fn collect(stream: pi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
	let mut events = Vec::new();
	let mut stream = stream;
	loop {
		let event = tokio::time::timeout(READ_TIMEOUT, stream.next())
			.await
			.expect("stream event within timeout");
		let Some(event) = event else {
			return events;
		};
		events.push(event);
	}
}

#[tokio::test]
async fn text_deltas_arrive_incrementally_and_assemble() {
	let gap = Duration::from_millis(80);
	let base = serve_sse_slow(
		vec![
			frame(r#"{"id":"chatcmpl-1","model":"gpt-test","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{"content":"lo "}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{"content":"world"}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
			frame(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#),
			"[DONE]\n\n".to_string(),
		],
		gap,
	)
	.await;

	let mut stream = stream_for(&base);
	let mut events = Vec::new();
	let mut delta_times: Vec<Instant> = Vec::new();
	loop {
		let event = tokio::time::timeout(READ_TIMEOUT, stream.next())
			.await
			.expect("stream event within timeout");
		let Some(event) = event else {
			break;
		};
		if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
			delta_times.push(Instant::now());
		}
		events.push(event);
	}

	assert!(
		matches!(events.first(), Some(AssistantMessageEvent::Start { .. })),
		"stream must start with a Start event"
	);
	let deltas: Vec<&str> = events
		.iter()
		.filter_map(|event| match event {
			AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
			_ => None,
		})
		.collect();
	assert_eq!(
		deltas,
		vec!["Hel", "lo ", "world"],
		"deltas must arrive one by one"
	);
	// Frames were spaced 80ms apart on the wire; all three deltas arriving
	// within less than one gap would mean the body was buffered whole.
	assert!(
		delta_times
			.last()
			.unwrap()
			.duration_since(*delta_times.first().unwrap())
			>= gap + Duration::from_millis(40),
		"text deltas must arrive spread out, not all at once"
	);

	// Partials grow monotonically to the full text.
	let mut previous = String::new();
	for event in &events {
		if let AssistantMessageEvent::TextDelta { partial, .. } = event {
			let text = partial.text();
			assert!(text.starts_with(&previous));
			assert!(text.len() > previous.len());
			previous = text;
		}
	}

	let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
		panic!(
			"stream must end with Done, got {:?}",
			events.last().map(|e| e.partial().stop_reason)
		);
	};
	assert_eq!(*reason, StopReason::Stop);
	assert_eq!(message.text(), "Hello world");
	assert_eq!(message.stop_reason, StopReason::Stop);
	assert_eq!(message.raw_stop_reason.as_deref(), Some("stop"));
	assert_eq!(message.response_id.as_deref(), Some("chatcmpl-1"));
	assert_eq!(message.response_model.as_deref(), Some("gpt-test"));
	assert_eq!(message.usage.input, 10);
	assert_eq!(message.usage.output, 5);
	assert_eq!(message.usage.total_tokens, 15);
}

#[tokio::test]
async fn tool_call_deltas_accumulate_into_complete_calls() {
	let base = serve_raw(vec![sse_response(
		&[
			frame(r#"{"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"ls","arguments":""}}]}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"cat","arguments":""}}]}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \".\"}"}}]}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"src\"}"}}]}}]}"#),
			frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
			"[DONE]\n\n".to_string(),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	let starts: Vec<usize> = events
		.iter()
		.filter_map(|event| match event {
			AssistantMessageEvent::ToolCallStart { content_index, .. } => Some(*content_index),
			_ => None,
		})
		.collect();
	assert_eq!(
		starts,
		vec![0, 1],
		"two parallel tool calls, independent indexes"
	);

	let delta_pieces: Vec<&str> = events
		.iter()
		.filter_map(|event| match event {
			AssistantMessageEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
			_ => None,
		})
		.collect();
	assert_eq!(
		delta_pieces,
		vec!["", "", "{\"pa", "th\": \".\"}", "{\"path\":\"src\"}"]
	);

	// By the time the second call starts, the first call's arguments preview
	// is not yet complete; the final parsed values are asserted on the Done
	// message above.
	let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
		panic!("stream must end with Done");
	};
	assert_eq!(*reason, StopReason::ToolUse);
	let calls = message.tool_calls();
	assert_eq!(calls.len(), 2);
	assert_eq!(calls[0].id, "call_a");
	assert_eq!(calls[0].name, "ls");
	assert_eq!(calls[0].arguments, serde_json::json!({"path": "."}));
	assert_eq!(calls[0].arguments_error, None);
	assert_eq!(calls[1].id, "call_b");
	assert_eq!(calls[1].name, "cat");
	assert_eq!(calls[1].arguments, serde_json::json!({"path": "src"}));
}

#[tokio::test]
async fn malformed_frame_switches_to_error_path_mid_stream() {
	let base = serve_raw(vec![sse_response(
		&[
			frame(r#"{"choices":[{"index":0,"delta":{"content":"partial text"}}]}"#),
			frame(r#"{"choices": [broken json"#),
			frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
			"[DONE]\n\n".to_string(),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	let text_deltas = events
		.iter()
		.filter(|event| matches!(event, AssistantMessageEvent::TextDelta { .. }))
		.count();
	assert_eq!(
		text_deltas, 1,
		"deltas before the malformed frame are delivered"
	);
	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("malformed frame must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	assert!(
		error
			.error_message
			.as_deref()
			.unwrap_or("")
			.contains("malformed SSE payload"),
		"error message should name the malformed frame, got {:?}",
		error.error_message
	);
	// The partially streamed text is preserved on the error message.
	assert_eq!(error.text(), "partial text");
}

#[tokio::test]
async fn dropped_connection_mid_stream_yields_error() {
	// Chunked body without the terminating record; the server then closes.
	let base = serve_raw(vec![sse_response(
		&[frame(
			r#"{"choices":[{"index":0,"delta":{"content":"kept"}}]}"#,
		)],
		false,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	assert!(events.iter().any(
		|event| matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "kept")
	));
	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("mid-stream disconnect must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	assert!(
		error
			.error_message
			.as_deref()
			.unwrap_or("")
			.contains("stream read failed"),
		"unexpected error message: {:?}",
		error.error_message
	);
}

#[tokio::test]
async fn http_error_status_maps_to_error_event() {
	let body = br#"{"error":{"message":"bad key"}}"#;
	let mut response = format!(
		"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
		body.len()
	)
	.into_bytes();
	response.extend_from_slice(body);
	let base = serve_raw(vec![response]).await;

	let events = collect(stream_for(&base)).await;

	assert_eq!(events.len(), 1, "no Start event may precede an HTTP error");
	let Some(AssistantMessageEvent::Error { reason, error }) = events.first() else {
		panic!("expected an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	assert_eq!(
		error.error_message.as_deref(),
		Some("provider returned 401 Unauthorized: bad key")
	);
}
