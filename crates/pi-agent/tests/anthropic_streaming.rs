//! End-to-end agent-loop tests over the real Anthropic Messages provider,
//! driven by a local mock HTTP+SSE server (raw `TcpListener`, chunked
//! transfer). Verifies that Anthropic content-block deltas surface as
//! `MessageUpdate`/`MessageEnd` agent events, tool calls assemble and execute,
//! and error/abort paths close the loop.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pi_agent::{Agent, AgentEvent, AgentOptions, AgentTool, AgentToolResult, ToolUpdate};
use pi_ai::{Message, Model, StopReason};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

struct ListTool;

#[async_trait]
impl AgentTool for ListTool {
	fn name(&self) -> &str {
		"ls"
	}
	fn description(&self) -> &str {
		"List files"
	}
	fn parameters_schema(&self) -> Value {
		json!({"type": "object"})
	}
	async fn execute(
		&self,
		_: &str,
		params: Value,
		_: CancellationToken,
		_: Option<ToolUpdate>,
	) -> AgentToolResult {
		assert_eq!(params["path"], ".");
		AgentToolResult::text("example.rs")
	}
}

/// The real provider stream fn with the API key injected, so the test stays
/// hermetic (no ANTHROPIC_API_KEY env dependency).
fn provider_stream_fn() -> pi_ai::StreamFn {
	let inner = pi_ai::providers::anthropic::stream_fn();
	Arc::new(move |model, context, mut options| {
		options.api_key = Some("test-key".to_string());
		inner(model, context, options)
	})
}

fn agent_at(base_url: &str, tools: Vec<Arc<dyn AgentTool>>) -> Agent {
	Agent::new(AgentOptions {
		model: Model::new(
			"claude-test",
			"claude-test",
			"anthropic-messages",
			"anthropic",
			base_url,
		),
		stream_fn: Some(provider_stream_fn()),
		tools,
		..Default::default()
	})
}

fn event(name: &str, payload: &str) -> String {
	format!("event: {name}\ndata: {payload}\n\n")
}

fn message_start() -> String {
	event(
		"message_start",
		r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":9,"output_tokens":0}}}"#,
	)
}

fn text_block(index: i64) -> String {
	event(
		"content_block_start",
		&json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}})
			.to_string(),
	)
}

fn text_delta(index: i64, text: &str) -> String {
	event(
		"content_block_delta",
		&json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}})
			.to_string(),
	)
}

fn block_stop(index: i64) -> String {
	event(
		"content_block_stop",
		&json!({"type":"content_block_stop","index":index}).to_string(),
	)
}

fn message_delta(stop_reason: &str) -> String {
	event(
		"message_delta",
		&json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":5}})
			.to_string(),
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

fn sse_response(frames: &[String]) -> Vec<u8> {
	let mut response =
		b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
			.to_vec();
	for frame in frames {
		response.extend_from_slice(&chunk_record(frame.as_bytes()));
	}
	response.extend_from_slice(b"0\r\n\r\n");
	response
}

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

/// Serves `responses` sequentially, one per agent turn, then closes.
async fn serve_turns(responses: Vec<Vec<u8>>) -> String {
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

#[tokio::test]
async fn anthropic_deltas_drive_message_updates_and_tool_round_trip() {
	let turn_one = sse_response(&[
		message_start(),
		text_block(0),
		text_delta(0, "Checking. "),
		block_stop(0),
		event(
			"content_block_start",
			r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"ls","input":{}}}"#,
		),
		event(
			"content_block_delta",
			r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
		),
		event(
			"content_block_delta",
			r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\".\"}"}}"#,
		),
		block_stop(1),
		message_delta("tool_use"),
		message_stop(),
	]);
	let turn_two = sse_response(&[
		message_start(),
		text_block(0),
		text_delta(0, "Found example.rs"),
		block_stop(0),
		message_delta("end_turn"),
		message_stop(),
	]);
	let base = serve_turns(vec![turn_one, turn_two]).await;

	let agent = agent_at(&base, vec![Arc::new(ListTool)]);
	let mut events = agent.subscribe();
	agent.prompt("list files".into()).await.unwrap();

	let snapshot = agent.snapshot();
	assert_eq!(
		snapshot
			.messages
			.iter()
			.map(Message::role)
			.collect::<Vec<_>>(),
		["system", "user", "assistant", "toolResult", "assistant"]
	);
	let Message::Assistant(calls) = &snapshot.messages[2] else {
		panic!("expected assistant with tool call")
	};
	assert_eq!(calls.stop_reason, StopReason::ToolUse);
	assert_eq!(calls.raw_stop_reason.as_deref(), Some("tool_use"));
	let tool_calls = calls.tool_calls();
	assert_eq!(tool_calls.len(), 1);
	assert_eq!(tool_calls[0].id, "toolu_1");
	assert_eq!(tool_calls[0].name, "ls");
	assert_eq!(tool_calls[0].arguments, json!({"path": "."}));
	let Message::ToolResult(result) = &snapshot.messages[3] else {
		panic!("expected tool result")
	};
	assert_eq!(result.tool_call_id, "toolu_1");
	assert_eq!(result.text(), "example.rs");
	let Message::Assistant(reply) = &snapshot.messages[4] else {
		panic!("expected final assistant")
	};
	assert_eq!(reply.text(), "Found example.rs");
	assert_eq!(reply.stop_reason, StopReason::Stop);
	// usage from message_start (input) + message_delta (output) of turn two.
	assert_eq!(reply.usage.input, 9);
	assert_eq!(reply.usage.output, 5);

	let mut updates = 0;
	let mut assistant_ends = 0;
	while let Ok(event) = events.try_recv() {
		match event {
			AgentEvent::MessageUpdate { message, .. } => {
				updates += 1;
				assert!(matches!(message, Message::Assistant(_)));
			}
			AgentEvent::MessageEnd { message } => {
				// Tool results end with a MessageEnd too; two of the four are
				// the streamed assistant turns.
				if matches!(message, Message::Assistant(_)) {
					assistant_ends += 1;
				}
			}
			_ => {}
		}
	}
	assert!(
		updates >= 6,
		"provider deltas must drive repeated MessageUpdate events, got {updates}"
	);
	assert_eq!(
		assistant_ends, 2,
		"each streamed assistant turn emits MessageEnd"
	);
}

#[tokio::test]
async fn thinking_then_text_streams_both_blocks_through_the_loop() {
	let turn = sse_response(&[
		message_start(),
		event(
			"content_block_start",
			r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
		),
		event(
			"content_block_delta",
			r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}"#,
		),
		block_stop(0),
		text_block(1),
		text_delta(1, "the answer"),
		block_stop(1),
		message_delta("end_turn"),
		message_stop(),
	]);
	let base = serve_turns(vec![turn]).await;

	let agent = agent_at(&base, vec![]);
	let mut events = agent.subscribe();
	agent.prompt("think then answer".into()).await.unwrap();

	let snapshot = agent.snapshot();
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant reply")
	};
	assert_eq!(reply.thinking(), "pondering");
	assert_eq!(reply.text(), "the answer");
	assert_eq!(reply.stop_reason, StopReason::Stop);

	let mut saw_thinking_update = false;
	while let Ok(event) = events.try_recv() {
		if let AgentEvent::MessageUpdate { message, .. } = event {
			let Message::Assistant(partial) = message else {
				continue;
			};
			saw_thinking_update |= !partial.thinking().is_empty();
		}
	}
	assert!(
		saw_thinking_update,
		"thinking deltas must surface as MessageUpdate events"
	);
}

#[tokio::test]
async fn sse_error_frame_mid_turn_keeps_partial_and_closes_loop() {
	let turn = sse_response(&[
		message_start(),
		text_block(0),
		text_delta(0, "still here"),
		event(
			"error",
			r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
		),
	]);
	let base = serve_turns(vec![turn]).await;

	let agent = agent_at(&base, vec![]);
	let mut events = agent.subscribe();
	agent.prompt("hello".into()).await.unwrap();

	let snapshot = agent.snapshot();
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant error message")
	};
	assert_eq!(reply.stop_reason, StopReason::Error);
	assert!(
		reply
			.error_message
			.as_deref()
			.unwrap_or("")
			.contains("overloaded_error"),
		"unexpected error message: {:?}",
		reply.error_message
	);
	assert_eq!(
		reply.text(),
		"still here",
		"partial output is kept on the error message"
	);

	let mut updates = 0;
	while let Ok(event) = events.try_recv() {
		if matches!(event, AgentEvent::MessageUpdate { .. }) {
			updates += 1;
		}
	}
	assert!(
		updates >= 1,
		"deltas before the error frame still stream as updates"
	);
}

#[tokio::test]
async fn abort_mid_stream_releases_agent_with_aborted_reason() {
	// A frame every 150ms; the test aborts after the first update.
	let mut frames = Vec::new();
	frames.push(message_start());
	frames.push(text_block(0));
	for index in 0..10 {
		frames.push(text_delta(0, &format!("piece{index} ")));
	}
	frames.push(block_stop(0));
	frames.push(message_delta("end_turn"));
	frames.push(message_stop());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let head =
		b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
	tokio::spawn(async move {
		let Ok((mut socket, _)) = listener.accept().await else {
			return;
		};
		drain_request(&mut socket).await;
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
			tokio::time::sleep(Duration::from_millis(150)).await;
		}
		let _ = socket.write_all(b"0\r\n\r\n").await;
	});
	let base = format!("http://{addr}");

	let agent = agent_at(&base, vec![]);
	let mut events = agent.subscribe();
	let runner = agent.clone();
	let task = tokio::spawn(async move { runner.prompt("stream slowly".into()).await });

	loop {
		match events.recv().await {
			Ok(AgentEvent::MessageUpdate { .. }) => break,
			Ok(_) => continue,
			Err(_) => panic!("agent event channel closed before first update"),
		}
	}
	agent.abort();
	tokio::time::timeout(Duration::from_secs(3), task)
		.await
		.expect("abort must release the loop promptly")
		.unwrap()
		.unwrap();
	assert!(!agent.is_streaming());
	let snapshot = agent.snapshot();
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant message")
	};
	assert_eq!(reply.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn thinking_end_event_carries_the_final_thinking_content() {
	let turn = sse_response(&[
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
			r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ing"}}"#,
		),
		block_stop(0),
		message_delta("end_turn"),
		message_stop(),
	]);
	let base = serve_turns(vec![turn]).await;

	let agent = agent_at(&base, vec![]);
	let mut events = agent.subscribe();
	agent.prompt("think".into()).await.unwrap();

	let mut thinking_end_content = None;
	while let Ok(event) = events.try_recv() {
		if let AgentEvent::MessageUpdate {
			assistant_message_event,
			..
		} = event
		{
			if assistant_message_event["type"] != "thinking_end" {
				continue;
			}
			assert_eq!(assistant_message_event["contentIndex"], 0);
			// The wire frame carries the authoritative final content of the
			// thinking block (upstream thinking_end.content).
			thinking_end_content = Some(
				assistant_message_event["content"]
					.as_str()
					.unwrap_or("")
					.to_string(),
			);
		}
	}
	assert_eq!(thinking_end_content.as_deref(), Some("pondering"));
}
