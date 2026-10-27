//! End-to-end streaming tests for the Anthropic Messages adapter against a
//! local mock HTTP+SSE server (raw `TcpListener`, chunked transfer encoding).
//! Exercises the incremental event loop ported from
//! `pi/packages/ai/src/api/anthropic-messages.ts`: block starts/deltas/stops,
//! tool-use fragment accumulation, usage and stop-reason mapping, and the
//! error paths (HTTP status, SSE `error` frames, disconnects, malformed
//! frames, missing `message_stop`, abort).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use pi_ai::stream::{AssistantMessageEvent, StreamOptions};
use pi_ai::{Message, Model, StopReason, normalize_context};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn model_at(base_url: &str) -> Model {
	Model::new(
		"claude-test",
		"claude-test",
		"anthropic-messages",
		"anthropic",
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
	(pi_ai::providers::anthropic::stream_fn())(&model_at(base_url), &context(), options())
}

/// One Anthropic SSE frame: `event:` name plus `data:` payload.
fn event(name: &str, payload: &str) -> String {
	format!("event: {name}\ndata: {payload}\n\n")
}

fn message_start() -> String {
	event(
		"message_start",
		r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":12,"output_tokens":0}}}"#,
	)
}

fn message_stop() -> String {
	event("message_stop", r#"{"type":"message_stop"}"#)
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

/// Reads one HTTP request (head plus content-length body) from the socket and
/// returns the raw bytes.
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

/// Serves one response and hands the captured request bytes to the test.
async fn serve_capture(response: Vec<u8>) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
	let sink = requests.clone();
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		let request = read_request(&mut socket).await;
		sink.lock().unwrap().push(request);
		let _ = socket.write_all(&response).await;
		let _ = socket.shutdown().await;
	});
	(format!("http://{addr}"), requests)
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
		read_request(&mut socket).await;
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

/// Streams `frames` slowly and never terminates the chunked body, so the
/// client stays mid-stream until the connection drops.
async fn serve_sse_stalled(frames: Vec<String>, gap: Duration) -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		read_request(&mut socket).await;
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
		// No terminating record: the server just closes mid-stream.
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
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo "}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
			),
			message_stop(),
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
	assert_eq!(deltas, vec!["Hel", "lo ", "world"]);

	// Frames were spaced 80ms apart on the wire; the first delta arriving
	// before the response body ended proves incremental delivery.
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

	assert!(events
		.iter()
		.any(|event| matches!(event, AssistantMessageEvent::TextEnd { content, .. } if content == "Hello world")));
	let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
		panic!("stream must end with Done");
	};
	assert_eq!(*reason, StopReason::Stop);
	assert_eq!(message.text(), "Hello world");
	assert_eq!(message.stop_reason, StopReason::Stop);
	assert_eq!(message.raw_stop_reason.as_deref(), Some("end_turn"));
	assert_eq!(message.response_id.as_deref(), Some("msg_1"));
	// input from message_start, output from message_delta.
	assert_eq!(message.usage.input, 12);
	assert_eq!(message.usage.output, 7);
	assert_eq!(message.usage.total_tokens, 19);
}

#[tokio::test]
async fn thinking_and_text_blocks_stream_in_order() {
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ponder"}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig9"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":1}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
			),
			message_stop(),
			// Ping frames and unknown events are skipped.
			event("ping", r#"{"type":"ping"}"#),
			event("something_new", r#"{"type":"something_new"}"#),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	let kinds: Vec<&str> = events
		.iter()
		.filter_map(|event| match event {
			AssistantMessageEvent::ThinkingStart { .. } => Some("thinking_start"),
			AssistantMessageEvent::ThinkingDelta { .. } => Some("thinking_delta"),
			AssistantMessageEvent::ThinkingEnd { .. } => Some("thinking_end"),
			AssistantMessageEvent::TextStart { .. } => Some("text_start"),
			AssistantMessageEvent::TextDelta { .. } => Some("text_delta"),
			AssistantMessageEvent::TextEnd { .. } => Some("text_end"),
			_ => None,
		})
		.collect();
	assert_eq!(
		kinds,
		vec![
			"thinking_start",
			"thinking_delta",
			"thinking_end",
			"text_start",
			"text_delta",
			"text_end",
		],
		"thinking block completes before the text block starts"
	);
	let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
		panic!("stream must end with Done");
	};
	assert_eq!(message.thinking(), "ponder");
	assert_eq!(message.text(), "answer");
	let signature = message
		.content
		.iter()
		.find_map(|block| match block {
			pi_ai::AssistantContent::Thinking(thinking) => thinking.thinking_signature.clone(),
			_ => None,
		})
		.unwrap();
	assert_eq!(
		signature, "sig9",
		"signature_delta appends without an event"
	);
}

#[tokio::test]
async fn tool_use_fragments_accumulate_by_index() {
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_a","name":"ls","input":{}}}"#,
			),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_b","name":"cat","input":{}}}"#,
			),
			// Index 1 is an interleaved text block; indexes interleave freely.
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"looking..."}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src\"}"}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\".\"}"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":1}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":2}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
			),
			message_stop(),
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
		"tool calls take content slots in arrival order; the later text block slots after them"
	);
	let deltas: Vec<&str> = events
		.iter()
		.filter_map(|event| match event {
			AssistantMessageEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
			_ => None,
		})
		.collect();
	assert_eq!(
		deltas,
		vec!["{\"pa", "{\"path\":\"src\"}", "th\":\".\"}"],
		"input_json_delta pieces are forwarded verbatim per block index"
	);
	let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
		panic!("stream must end with Done");
	};
	assert_eq!(*reason, StopReason::ToolUse);
	assert_eq!(message.stop_reason, StopReason::ToolUse);
	assert_eq!(message.raw_stop_reason.as_deref(), Some("tool_use"));
	let calls = message.tool_calls();
	assert_eq!(calls.len(), 2);
	assert_eq!(calls[0].id, "toolu_a");
	assert_eq!(calls[0].name, "ls");
	assert_eq!(calls[0].arguments, json!({"path": "."}));
	assert_eq!(calls[0].arguments_error, None);
	assert_eq!(calls[1].id, "toolu_b");
	assert_eq!(calls[1].name, "cat");
	assert_eq!(calls[1].arguments, json!({"path": "src"}));
}

#[tokio::test]
async fn malformed_tool_arguments_surface_as_arguments_error() {
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"ls","input":{}}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
			),
			message_stop(),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
		panic!("stream must end with Done");
	};
	let calls = message.tool_calls();
	assert_eq!(calls[0].arguments, json!({}));
	assert!(
		calls[0]
			.arguments_error
			.as_deref()
			.unwrap()
			.contains("Received arguments:"),
		"same arguments_error semantics as the OpenAI adapter"
	);
}

#[tokio::test]
async fn usage_and_stop_reasons_map_with_cache_tokens() {
	let base = serve_raw(vec![sse_response(
		&[
			event(
				"message_start",
				r#"{"type":"message_start","message":{"id":"msg_u","model":"claude-other","usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":40,"cache_creation_input_tokens":25,"cache_creation":{"ephemeral_1h_input_tokens":10}}}}"#,
			),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"input_tokens":null,"output_tokens":60,"cache_read_input_tokens":55,"cache_creation_input_tokens":30,"output_tokens_details":{"thinking_tokens":9}}}"#,
			),
			message_stop(),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
		panic!("stream must end with Done");
	};
	assert_eq!(*reason, StopReason::Length);
	assert_eq!(message.raw_stop_reason.as_deref(), Some("max_tokens"));
	// The response model differs from the requested one and is recorded.
	assert_eq!(message.response_model.as_deref(), Some("claude-other"));
	// message_delta usage overrides message_start where present; null input
	// keeps the message_start value.
	assert_eq!(message.usage.input, 100);
	assert_eq!(message.usage.output, 60);
	assert_eq!(message.usage.cache_read, 55);
	assert_eq!(message.usage.cache_write, 30);
	assert_eq!(message.usage.cache_write_1h, Some(10));
	assert_eq!(message.usage.reasoning, Some(9));
	assert_eq!(message.usage.total_tokens, 100 + 60 + 55 + 30);
}

#[tokio::test]
async fn refusal_stop_reason_becomes_error_with_partial() {
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I cannot"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"explanation":"not allowed"}}}"#,
			),
			message_stop(),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	// refusal maps to an error stop reason, which upstream surfaces through
	// the error path carrying the live output.
	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("refusal must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	assert_eq!(error.error_message.as_deref(), Some("not allowed"));
	assert_eq!(error.text(), "I cannot", "partial output is preserved");
}

#[tokio::test]
async fn http_error_with_anthropic_body_maps_to_error_event() {
	let body = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
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
		Some("provider returned 401 Unauthorized: invalid x-api-key")
	);
}

#[tokio::test]
async fn sse_error_event_yields_error_carrying_partial() {
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"kept"}}"#,
			),
			event(
				"error",
				r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
			),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	assert!(events.iter().any(
		|event| matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "kept")
	));
	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("SSE error frame must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	// Upstream throws with the raw SSE payload as the message.
	assert!(
		error
			.error_message
			.as_deref()
			.unwrap_or("")
			.contains("overloaded_error"),
		"unexpected error message: {:?}",
		error.error_message
	);
	assert_eq!(error.text(), "kept", "partial output is preserved");
}

#[tokio::test]
async fn malformed_frame_switches_to_error_path_mid_stream() {
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial text"}}"#,
			),
			event("content_block_delta", r#"{"type": [broken json"#),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
			),
			message_stop(),
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
	let message = error.error_message.as_deref().unwrap_or("");
	assert!(
		message.contains("Could not parse Anthropic SSE event content_block_delta"),
		"error message should name the event, got {message}"
	);
	assert!(message.contains("broken json"), "raw payload is quoted");
	assert_eq!(error.text(), "partial text", "partial is preserved");
}

#[tokio::test]
async fn disconnect_before_message_stop_yields_error_with_partial() {
	// message_start arrives, then the connection drops before message_stop.
	let base = serve_sse_stalled(
		vec![
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"truncated"}}"#,
			),
		],
		Duration::from_millis(30),
	)
	.await;

	let events = collect(stream_for(&base)).await;

	assert!(events.iter().any(
		|event| matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "truncated")
	));
	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("mid-stream disconnect must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	let message = error.error_message.as_deref().unwrap_or("");
	assert!(
		message.contains("stream read failed") || message.contains("before message_stop"),
		"unexpected error message: {message}"
	);
	assert_eq!(error.text(), "truncated", "partial is preserved");
}

#[tokio::test]
async fn clean_close_without_message_stop_or_stop_reason_fails() {
	// Full body, clean EOF, but the provider never sent message_stop nor a
	// stop reason: the message_stop guard fires first.
	let base = serve_raw(vec![sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}"#,
			),
		],
		true,
	)])
	.await;

	let events = collect(stream_for(&base)).await;

	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("missing message_stop must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Error);
	assert_eq!(
		error.error_message.as_deref(),
		Some("Anthropic stream ended before message_stop")
	);
}

#[tokio::test]
async fn abort_cancels_the_stream_immediately() {
	let mut frames = Vec::new();
	frames.push(message_start());
	for index in 0..10 {
		frames.push(event(
			"content_block_start",
			r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
		));
		frames.push(event(
			"content_block_delta",
			&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":format!("piece{index} ")}})
				.to_string(),
		));
	}
	let base = serve_sse_stalled(frames, Duration::from_millis(120)).await;

	let signal = tokio_util::sync::CancellationToken::new();
	let mut stream = (pi_ai::providers::anthropic::stream_fn())(
		&model_at(&base),
		&context(),
		StreamOptions {
			api_key: Some("test-key".to_string()),
			signal: signal.clone(),
			..Default::default()
		},
	);
	let mut events = Vec::new();
	loop {
		let event = tokio::time::timeout(READ_TIMEOUT, stream.next())
			.await
			.expect("stream event within timeout");
		let Some(event) = event else {
			break;
		};
		let got_delta = matches!(event, AssistantMessageEvent::TextDelta { .. });
		events.push(event);
		if got_delta {
			signal.cancel();
		}
	}
	let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
		panic!("abort must end the stream with an Error event");
	};
	assert_eq!(*reason, StopReason::Aborted);
	assert_eq!(error.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn request_wire_format_reaches_v1_messages() {
	let response = sse_response(
		&[
			message_start(),
			event(
				"content_block_start",
				r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
			),
			event(
				"content_block_delta",
				r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
			),
			event(
				"content_block_stop",
				r#"{"type":"content_block_stop","index":0}"#,
			),
			event(
				"message_delta",
				r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
			),
			message_stop(),
		],
		true,
	);
	let (base, requests) = serve_capture(response).await;

	let model = {
		let mut model = model_at(&base);
		model.max_tokens = 2048;
		model
	};
	let stream = (pi_ai::providers::anthropic::stream_fn())(
		&model,
		&context(),
		StreamOptions {
			api_key: Some("test-key".to_string()),
			max_tokens: Some(512),
			temperature: Some(0.5),
			..Default::default()
		},
	);
	let events = collect(stream).await;
	assert!(
		matches!(events.last(), Some(AssistantMessageEvent::Done { .. })),
		"stream completes"
	);

	let requests = requests.lock().unwrap();
	assert_eq!(requests.len(), 1);
	let raw = String::from_utf8_lossy(&requests[0]).to_string();
	let (head, body) = raw.split_once("\r\n\r\n").expect("request head and body");
	assert!(head.starts_with("POST /v1/messages"), "head: {head}");
	let head_lower = head.to_lowercase();
	assert!(
		head_lower.contains("x-api-key: test-key"),
		"auth header missing: {head}"
	);
	assert!(
		head_lower.contains("anthropic-version: 2023-06-01"),
		"version header missing: {head}"
	);
	let body: Value = serde_json::from_str(body).expect("JSON body");
	assert_eq!(body["model"], "claude-test");
	assert_eq!(body["stream"], true);
	assert_eq!(body["max_tokens"], 512, "explicit max_tokens wins");
	assert_eq!(body["temperature"], 0.5);
	assert_eq!(
		body["system"],
		json!([{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}]),
		"system prompt is top-level"
	);
	assert_eq!(
		body["messages"],
		json!([{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]}]),
		"final user message carries the cache breakpoint (string content becomes a block)"
	);
	assert!(body.get("tools").is_none());
	assert!(body.get("thinking").is_none());
}
