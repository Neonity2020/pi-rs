//! End-to-end agent-loop tests over the real OpenAI-compatible provider,
//! driven by a local mock HTTP+SSE server (raw `TcpListener`, chunked
//! transfer). Verifies that provider deltas surface as `MessageUpdate` agent
//! events, tool calls assemble and execute, and error paths close the loop.

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
/// hermetic (no OPENAI_API_KEY env dependency).
fn provider_stream_fn() -> pi_ai::StreamFn {
	let inner = pi_ai::providers::openai_completions::stream_fn();
	Arc::new(move |model, context, mut options| {
		options.api_key = Some("test-key".to_string());
		inner(model, context, options)
	})
}

fn agent_at(base_url: &str, tools: Vec<Arc<dyn AgentTool>>) -> Agent {
	Agent::new(AgentOptions {
		model: Model::new(
			"test-model",
			"test",
			"openai-completions",
			"openai",
			base_url,
		),
		stream_fn: Some(provider_stream_fn()),
		tools,
		..Default::default()
	})
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

fn text_turn(text: &str, finish: &str) -> Vec<u8> {
	sse_response(&[
		frame(
			&json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text}}]})
				.to_string(),
		),
		frame(&json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}).to_string()),
		"[DONE]\n\n".to_string(),
	])
}

#[tokio::test]
async fn provider_deltas_surface_as_message_updates_and_tool_round_trip() {
	let turn_one = sse_response(&[
		frame(r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Checking. "}}]}"#),
		frame(
			r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"ls","arguments":""}}]}}]}"#,
		),
		frame(
			r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
		),
		frame(
			r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\".\"}"}}]}}]}"#,
		),
		frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
		"[DONE]\n\n".to_string(),
	]);
	let base = serve_turns(vec![turn_one, text_turn("Found example.rs", "stop")]).await;

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
	let tool_calls = calls.tool_calls();
	assert_eq!(tool_calls.len(), 1);
	assert_eq!(tool_calls[0].id, "call_1");
	assert_eq!(tool_calls[0].name, "ls");
	assert_eq!(tool_calls[0].arguments, json!({"path": "."}));
	let Message::ToolResult(result) = &snapshot.messages[3] else {
		panic!("expected tool result")
	};
	assert_eq!(result.tool_call_id, "call_1");
	assert_eq!(result.text(), "example.rs");
	let Message::Assistant(reply) = &snapshot.messages[4] else {
		panic!("expected final assistant")
	};
	assert_eq!(reply.text(), "Found example.rs");
	assert_eq!(reply.stop_reason, StopReason::Stop);

	let mut updates = 0;
	while let Ok(event) = events.try_recv() {
		if matches!(event, AgentEvent::MessageUpdate { .. }) {
			updates += 1;
		}
	}
	assert!(
		updates >= 6,
		"provider deltas must drive repeated MessageUpdate events, got {updates}"
	);
}

#[tokio::test]
async fn malformed_frame_mid_turn_keeps_partial_text_and_errors() {
	let turn = sse_response(&[
		frame(r#"{"choices":[{"index":0,"delta":{"content":"still here"}}]}"#),
		frame(r#"{"choices": [not json"#),
		"[DONE]\n\n".to_string(),
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
			.contains("malformed SSE payload")
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
		"deltas before the malformed frame still stream as updates"
	);
}

#[tokio::test]
async fn abort_mid_stream_releases_agent_with_aborted_reason() {
	// A frame every 150ms; the test aborts after the first update.
	let mut frames = Vec::new();
	for index in 0..10 {
		frames.push(frame(
			&json!({"choices":[{"index":0,"delta":{"content":format!("piece{index} ")}}]})
				.to_string(),
		));
	}
	frames.push("[DONE]\n\n".to_string());
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
	// reasoning_content deltas fold into a thinking block; the loop's
	// message_update frame for thinking_end must carry its final content.
	let turn = sse_response(&[
		frame(
			r#"{"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"ponder"}}]}"#,
		),
		frame(
			r#"{"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"ing"}}]}"#,
		),
		frame(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
		"[DONE]\n\n".to_string(),
	]);
	let base = serve_turns(vec![turn]).await;

	let agent = agent_at(&base, vec![]);
	let mut events = agent.subscribe();
	agent.prompt("think".into()).await.unwrap();

	let snapshot = agent.snapshot();
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant")
	};
	assert_eq!(reply.thinking(), "pondering");

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
