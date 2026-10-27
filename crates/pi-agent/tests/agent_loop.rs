use std::sync::Arc;

use async_trait::async_trait;
use pi_agent::{Agent, AgentEvent, AgentOptions, AgentTool, AgentToolResult, ToolUpdate};
use pi_ai::{Message, Model, StopReason};
use serde_json::{Value, json};
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

fn mock_agent(tools: Vec<Arc<dyn AgentTool>>) -> Agent {
	Agent::new(AgentOptions {
		model: Model::new("mock", "mock", "mock", "mock", ""),
		stream_fn: Some(Arc::new(pi_ai::providers::mock::stream)),
		tools,
		..Default::default()
	})
}

#[tokio::test]
async fn tool_call_round_trip_preserves_result_and_events() {
	let agent = mock_agent(vec![Arc::new(ListTool)]);
	let mut events = agent.subscribe();
	agent.prompt("list files".into()).await.unwrap();
	let snapshot = agent.snapshot();
	assert!(!agent.is_streaming());
	assert_eq!(
		snapshot
			.messages
			.iter()
			.map(Message::role)
			.collect::<Vec<_>>(),
		["system", "user", "assistant", "toolResult", "assistant"]
	);
	let Message::ToolResult(result) = &snapshot.messages[3] else {
		panic!("expected tool result")
	};
	assert!(!result.is_error);
	assert_eq!(result.text(), "example.rs");
	let Message::Assistant(reply) = &snapshot.messages[4] else {
		panic!("expected assistant response")
	};
	assert!(reply.text().contains("example.rs"));
	let mut kinds = Vec::new();
	while let Ok(event) = events.try_recv() {
		kinds.push(event.kind());
	}
	assert_eq!(kinds.first(), Some(&"agent_start"));
	assert_eq!(kinds.last(), Some(&"agent_end"));
	assert!(kinds.contains(&"tool_execution_start"));
	assert!(kinds.contains(&"tool_execution_end"));
}

#[tokio::test]
async fn unknown_tool_is_returned_as_error_and_loop_continues() {
	let agent = mock_agent(vec![]);
	agent.prompt("list files".into()).await.unwrap();
	let snapshot = agent.snapshot();
	let Message::ToolResult(result) = &snapshot.messages[3] else {
		panic!("expected tool result")
	};
	assert!(result.is_error);
	assert!(result.text().contains("unknown tool: ls"));
	assert!(matches!(
		snapshot.messages.last(),
		Some(Message::Assistant(_))
	));
}

#[tokio::test]
async fn queued_follow_up_runs_after_first_answer() {
	let agent = mock_agent(vec![]);
	agent.follow_up("second question");
	agent.prompt("first question".into()).await.unwrap();
	let snapshot = agent.snapshot();
	assert_eq!(
		snapshot
			.messages
			.iter()
			.map(Message::role)
			.collect::<Vec<_>>(),
		["system", "user", "assistant", "user", "assistant"]
	);
	assert!(!agent.has_queued_messages());
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant")
	};
	assert!(reply.text().contains("second question"));
}

#[tokio::test]
async fn abort_stops_stream_and_releases_agent() {
	let agent = mock_agent(vec![]);
	let mut events = agent.subscribe();
	let running = agent.clone();
	let task = tokio::spawn(async move { running.prompt("a long message to stream".into()).await });
	loop {
		if matches!(
			events.recv().await.unwrap(),
			AgentEvent::MessageUpdate { .. }
		) {
			break;
		}
	}
	agent.abort();
	tokio::time::timeout(std::time::Duration::from_secs(2), task)
		.await
		.unwrap()
		.unwrap()
		.unwrap();
	assert!(!agent.is_streaming());
	let snapshot = agent.snapshot();
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant")
	};
	assert_eq!(reply.stop_reason, StopReason::Aborted);
}
