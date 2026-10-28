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
async fn panic_releases_active_run() {
	let agent = Agent::new(AgentOptions {
		stream_fn: Some(Arc::new(|_, _, _| panic!("provider panic"))),
		..Default::default()
	});
	let worker = agent.clone();
	assert!(
		tokio::spawn(async move { worker.prompt("hello".into()).await })
			.await
			.is_err()
	);
	assert!(!agent.is_streaming());
	agent.set_stream_fn(Arc::new(pi_ai::providers::mock::stream));
	agent.prompt("again".into()).await.unwrap();
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

struct UpdatingTool {
	callback: std::sync::Mutex<Option<ToolUpdate>>,
}

#[async_trait]
impl AgentTool for UpdatingTool {
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
		_: Value,
		_: CancellationToken,
		on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let callback = on_update.expect("update callback");
		let mut partial = AgentToolResult::text("partial");
		partial.details = Some(json!({"count": 1}));
		partial.structured_content = Some(json!({"item": "a"}));
		callback(partial);
		*self.callback.lock().unwrap() = Some(callback);
		let mut result = AgentToolResult::text("done");
		result.structured_content = Some(json!({"final": true}));
		result
	}
}

#[tokio::test]
async fn tool_updates_preserve_result_and_stop_after_execution() {
	let tool = Arc::new(UpdatingTool {
		callback: std::sync::Mutex::new(None),
	});
	let agent = mock_agent(vec![tool.clone()]);
	let mut events = agent.subscribe();
	agent.prompt("list files".into()).await.unwrap();
	tool.callback.lock().unwrap().as_ref().unwrap()(AgentToolResult::text("late"));
	let mut lifecycle = Vec::new();
	while let Ok(event) = events.try_recv() {
		match event {
			AgentEvent::ToolExecutionStart { .. } => lifecycle.push("start"),
			AgentEvent::ToolExecutionUpdate {
				tool_name,
				args,
				partial_result,
				..
			} => {
				lifecycle.push("update");
				assert_eq!(tool_name, "ls");
				assert_eq!(args["path"], ".");
				assert_eq!(partial_result["content"][0]["text"], "partial");
				assert_eq!(partial_result["details"]["count"], 1);
				assert_eq!(partial_result["structuredContent"]["item"], "a");
			}
			AgentEvent::ToolExecutionEnd { result, .. } => {
				lifecycle.push("end");
				assert_eq!(result["content"][0], json!({"type":"text", "text":"done"}));
				assert_eq!(result["structuredContent"]["final"], true);
			}
			_ => {}
		}
	}
	assert_eq!(lifecycle, ["start", "update", "end"]);
}

// ---------------------------------------------------------------------------
// Scripted provider + shared helpers for the P1 hook / parallel coverage.
// ---------------------------------------------------------------------------

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pi_agent::{
	AfterToolCallResult, AgentRequestUpdate, AgentTurnDecision, BeforeToolCallResult, QueueMode,
};
use pi_ai::{
	AssistantContent, AssistantMessage, AssistantMessageEvent, StreamOptions, TextContent,
	TextOrImage, ThinkingLevel, ToolCall, TranscriptContext, Usage,
};

/// One observed tool lifecycle event, in emission order.
#[derive(Debug, PartialEq)]
enum ToolEvent {
	Start(String),
	End(String, bool, Option<u64>),
}

fn collect_tool_events(
	events: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
) -> Vec<ToolEvent> {
	let mut observed = Vec::new();
	while let Ok(event) = events.try_recv() {
		match event {
			AgentEvent::ToolExecutionStart { tool_name, .. } => {
				observed.push(ToolEvent::Start(tool_name))
			}
			AgentEvent::ToolExecutionEnd {
				tool_name,
				is_error,
				duration_ms,
				..
			} => {
				// Canonicalize positive durations to Some(0): the value itself
				// is timing-dependent, only presence matters to the assertions.
				observed.push(ToolEvent::End(tool_name, is_error, duration_ms.map(|_| 0)))
			}
			_ => {}
		}
	}
	observed
}

fn tool_result_texts(snapshot: &pi_agent::AgentSnapshot) -> Vec<String> {
	snapshot
		.messages
		.iter()
		.filter_map(|message| match message {
			Message::ToolResult(result) => Some(result.text()),
			_ => None,
		})
		.collect()
}

fn roles_of(snapshot: &pi_agent::AgentSnapshot) -> Vec<&str> {
	snapshot.messages.iter().map(Message::role).collect()
}

/// Content blocks of one scripted assistant response.
enum ScriptBlock {
	Text(String),
	ToolCall(ToolCall),
}

fn tool_call(id: &str, name: &str) -> ToolCall {
	ToolCall {
		id: id.into(),
		name: name.into(),
		arguments: json!({}),
		thought_signature: None,
		namespace: None,
		arguments_error: None,
	}
}

/// Builds a complete, protocol-correct event sequence for one response. Called
/// at request time so message timestamps look fresh to the loop.
fn scripted_turn(
	model: &Model,
	stop_reason: StopReason,
	blocks: Vec<ScriptBlock>,
) -> Vec<AssistantMessageEvent> {
	let base = AssistantMessage {
		content: Vec::new(),
		api: model.api.as_str().to_string(),
		provider: model.provider.clone(),
		model: model.id.clone(),
		response_model: None,
		response_id: None,
		thinking_level: None,
		usage: Usage::default(),
		stop_reason,
		error_message: None,
		raw_stop_reason: None,
		end_turn: None,
		timestamp: pi_ai::types::now_ms(),
		duration_ms: None,
	};
	let mut partial = base;
	let mut events = vec![AssistantMessageEvent::Start {
		partial: partial.clone(),
	}];
	for block in blocks {
		match block {
			ScriptBlock::Text(text) => {
				let index = partial.content.len();
				partial.content.push(AssistantContent::Text(TextContent {
					text: String::new(),
					text_signature: None,
				}));
				events.push(AssistantMessageEvent::TextStart {
					content_index: index,
					partial: partial.clone(),
				});
				events.push(AssistantMessageEvent::TextDelta {
					content_index: index,
					delta: text.clone(),
					partial: partial.clone(),
				});
				if let Some(AssistantContent::Text(t)) = partial.content.last_mut() {
					t.text = text.clone();
				}
				events.push(AssistantMessageEvent::TextEnd {
					content_index: index,
					content: text.clone(),
					partial: partial.clone(),
				});
			}
			ScriptBlock::ToolCall(call) => {
				let index = partial.content.len();
				partial
					.content
					.push(AssistantContent::ToolCall(call.clone()));
				events.push(AssistantMessageEvent::ToolCallStart {
					content_index: index,
					partial: partial.clone(),
				});
				events.push(AssistantMessageEvent::ToolCallEnd {
					content_index: index,
					tool_call: call,
					partial: partial.clone(),
				});
			}
		}
	}
	events.push(AssistantMessageEvent::Done {
		reason: stop_reason,
		message: partial,
	});
	events
}

fn text_turn(text: &str) -> (StopReason, Vec<ScriptBlock>) {
	(StopReason::Stop, vec![ScriptBlock::Text(text.to_string())])
}

/// Stream fn serving scripted responses in order; records (model id, reasoning)
/// per request.
fn scripted_stream_fn(
	turns: Vec<(StopReason, Vec<ScriptBlock>)>,
	log: RequestLog,
) -> pi_ai::StreamFn {
	let queue = Arc::new(StdMutex::new(VecDeque::from(turns)));
	Arc::new(
		move |model: &Model, _: &TranscriptContext, options: StreamOptions| {
			log.lock()
				.unwrap()
				.push((model.id.clone(), options.reasoning));
			let (stop_reason, blocks) = queue
				.lock()
				.unwrap()
				.pop_front()
				.unwrap_or_else(|| panic!("scripted stream exhausted"));
			let events = scripted_turn(model, stop_reason, blocks);
			Box::pin(futures::stream::iter(events)) as pi_ai::AssistantMessageEventStream
		},
	)
}

type RequestLog = Arc<StdMutex<Vec<(String, Option<ThinkingLevel>)>>>;

fn scripted_agent(
	turns: Vec<(StopReason, Vec<ScriptBlock>)>,
	tools: Vec<Arc<dyn AgentTool>>,
) -> (Agent, RequestLog) {
	let log: RequestLog = Arc::default();
	let agent = Agent::new(AgentOptions {
		model: Model::new("scripted", "scripted", "scripted", "scripted", ""),
		stream_fn: Some(scripted_stream_fn(turns, log.clone())),
		tools,
		..Default::default()
	});
	(agent, log)
}

/// Tool that records executions and can chain completion order through Notifies.
struct GatedTool {
	name: &'static str,
	log: Arc<StdMutex<Vec<String>>>,
	wait_for: Option<Arc<tokio::sync::Notify>>,
	notify: Option<Arc<tokio::sync::Notify>>,
	sequential: bool,
}

#[async_trait]
impl AgentTool for GatedTool {
	fn name(&self) -> &str {
		self.name
	}
	fn description(&self) -> &str {
		"gated tool"
	}
	fn parameters_schema(&self) -> Value {
		json!({"type": "object"})
	}
	fn execution_mode(&self) -> Option<pi_agent::ToolExecutionMode> {
		self.sequential
			.then_some(pi_agent::ToolExecutionMode::Sequential)
	}
	async fn execute(
		&self,
		_: &str,
		_: Value,
		_: CancellationToken,
		_: Option<ToolUpdate>,
	) -> AgentToolResult {
		if let Some(wait) = &self.wait_for {
			wait.notified().await;
		}
		self.log.lock().unwrap().push(self.name.to_string());
		if let Some(open) = &self.notify {
			open.notify_one();
		}
		AgentToolResult::text(self.name)
	}
}

fn execution_log() -> Arc<StdMutex<Vec<String>>> {
	Arc::default()
}

fn gated_tool(
	name: &'static str,
	log: &Arc<StdMutex<Vec<String>>>,
	wait_for: Option<Arc<tokio::sync::Notify>>,
	notify: Option<Arc<tokio::sync::Notify>>,
) -> Arc<dyn AgentTool> {
	Arc::new(GatedTool {
		name,
		log: log.clone(),
		wait_for,
		notify,
		sequential: false,
	})
}

/// A gated tool that declares the sequential execution mode.
fn sequential_tool(name: &'static str, log: &Arc<StdMutex<Vec<String>>>) -> Arc<dyn AgentTool> {
	Arc::new(GatedTool {
		name,
		log: log.clone(),
		wait_for: None,
		notify: None,
		sequential: true,
	})
}

// --- parallel tool execution -------------------------------------------------

#[tokio::test]
async fn parallel_tools_start_in_source_order_end_in_completion_order_results_in_source_order() {
	let beta_done = Arc::new(tokio::sync::Notify::new());
	let alpha_done = Arc::new(tokio::sync::Notify::new());
	let log = execution_log();
	let tools = vec![
		// alpha waits for beta, then releases gamma: completion order is
		// beta, alpha, gamma while source order stays alpha, beta, gamma.
		gated_tool(
			"alpha",
			&log,
			Some(beta_done.clone()),
			Some(alpha_done.clone()),
		),
		gated_tool("beta", &log, None, Some(beta_done)),
		gated_tool("gamma", &log, Some(alpha_done), None),
	];
	let (agent, _) = scripted_agent(
		vec![
			(
				StopReason::ToolUse,
				vec![
					ScriptBlock::ToolCall(tool_call("a1", "alpha")),
					ScriptBlock::ToolCall(tool_call("b1", "beta")),
					ScriptBlock::ToolCall(tool_call("g1", "gamma")),
				],
			),
			text_turn("done"),
		],
		tools,
	);
	let mut events = agent.subscribe();
	tokio::time::timeout(Duration::from_secs(5), agent.prompt("run".into()))
		.await
		.expect("no deadlock: parallel execution must interleave")
		.unwrap();

	let observed = collect_tool_events(&mut events);
	assert_eq!(
		observed,
		[
			ToolEvent::Start("alpha".into()),
			ToolEvent::Start("beta".into()),
			ToolEvent::Start("gamma".into()),
			ToolEvent::End("beta".into(), false, Some(0)),
			ToolEvent::End("alpha".into(), false, Some(0)),
			ToolEvent::End("gamma".into(), false, Some(0)),
		],
		"all starts are emitted in source order before execution; ends follow completion order"
	);
	assert_eq!(
		tool_result_texts(&agent.snapshot()),
		["alpha", "beta", "gamma"],
		"tool result messages follow source order"
	);
	assert_eq!(
		*log.lock().unwrap(),
		["beta", "alpha", "gamma"],
		"executions overlap: completion order, not source order"
	);
}

#[tokio::test]
async fn sequential_tool_declaration_forces_the_sequential_path_for_the_batch() {
	let log = execution_log();
	let tools = vec![
		sequential_tool("solo", &log),
		gated_tool("beta", &log, None, None),
	];
	let (agent, _) = scripted_agent(
		vec![
			(
				StopReason::ToolUse,
				vec![
					ScriptBlock::ToolCall(tool_call("s1", "solo")),
					ScriptBlock::ToolCall(tool_call("b1", "beta")),
				],
			),
			text_turn("done"),
		],
		tools,
	);
	let mut events = agent.subscribe();
	agent.prompt("run".into()).await.unwrap();

	// Sequential interleaving: each call is fully finalized (end + result
	// message) before the next one starts.
	let observed = collect_tool_events(&mut events);
	assert_eq!(
		observed,
		[
			ToolEvent::Start("solo".into()),
			ToolEvent::End("solo".into(), false, Some(0)),
			ToolEvent::Start("beta".into()),
			ToolEvent::End("beta".into(), false, Some(0)),
		]
	);
	assert_eq!(*log.lock().unwrap(), ["solo", "beta"]);
}

// --- before_tool_call hook ---------------------------------------------------

#[tokio::test]
async fn before_tool_call_block_replaces_execution_with_error_result() {
	let log = execution_log();
	let (agent, _) = scripted_agent(
		vec![
			(
				StopReason::ToolUse,
				vec![ScriptBlock::ToolCall(tool_call("t1", "ls"))],
			),
			text_turn("after block"),
		],
		vec![gated_tool("ls", &log, None, None)],
	);
	agent.set_before_tool_call(Arc::new(|context| BeforeToolCallResult {
		block: context.tool_call.name == "ls",
		reason: Some("not allowed".into()),
		..Default::default()
	}));
	let mut events = agent.subscribe();
	agent.prompt("run".into()).await.unwrap();

	assert!(
		log.lock().unwrap().is_empty(),
		"blocked tools must not execute"
	);
	assert_eq!(tool_result_texts(&agent.snapshot()), ["not allowed"]);
	assert_eq!(
		collect_tool_events(&mut events),
		[
			ToolEvent::Start("ls".into()),
			ToolEvent::End("ls".into(), true, None),
		],
		"blocked calls still emit start, then an error end without a duration"
	);
	// The loop continues: the model sees the error result and answers again.
	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "toolResult", "assistant"]
	);
}

#[tokio::test]
async fn before_tool_call_block_without_reason_uses_the_default_message() {
	let log = execution_log();
	let (agent, _) = scripted_agent(
		vec![
			(
				StopReason::ToolUse,
				vec![ScriptBlock::ToolCall(tool_call("t1", "ls"))],
			),
			text_turn("after block"),
		],
		vec![gated_tool("ls", &log, None, None)],
	);
	agent.set_before_tool_call(Arc::new(|_| BeforeToolCallResult {
		block: true,
		..Default::default()
	}));
	agent.prompt("run".into()).await.unwrap();
	assert_eq!(
		tool_result_texts(&agent.snapshot()),
		["Tool execution was blocked"]
	);
}

#[tokio::test]
async fn before_tool_call_terminate_stops_launching_remaining_parallel_calls() {
	let log = execution_log();
	let (agent, _) = scripted_agent(
		vec![(
			StopReason::ToolUse,
			vec![
				ScriptBlock::ToolCall(tool_call("a1", "alpha")),
				ScriptBlock::ToolCall(tool_call("b1", "beta")),
			],
		)],
		vec![
			gated_tool("alpha", &log, None, None),
			gated_tool("beta", &log, None, None),
		],
	);
	agent.set_before_tool_call(Arc::new(|context| BeforeToolCallResult {
		block: context.tool_call.name == "alpha",
		reason: Some("stop everything".into()),
		terminate: true,
	}));
	agent.prompt("run".into()).await.unwrap();

	assert!(log.lock().unwrap().is_empty(), "neither tool may execute");
	let texts = tool_result_texts(&agent.snapshot());
	assert_eq!(texts.len(), 2);
	assert_eq!(texts[0], "stop everything");
	assert!(
		texts[1].contains("requested termination"),
		"later calls are skipped with an error result, got: {}",
		texts[1]
	);
	// The terminate hint ends the run: no second assistant turn is scripted.
	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "toolResult", "toolResult"]
	);
}

// --- after_tool_call hook ----------------------------------------------------

struct FailingTool;

#[async_trait]
impl AgentTool for FailingTool {
	fn name(&self) -> &str {
		"ls"
	}
	fn description(&self) -> &str {
		"failing tool"
	}
	fn parameters_schema(&self) -> Value {
		json!({"type": "object"})
	}
	async fn execute(
		&self,
		_: &str,
		_: Value,
		_: CancellationToken,
		_: Option<ToolUpdate>,
	) -> AgentToolResult {
		let mut result = AgentToolResult::text("boom");
		result.is_error = true;
		result.details = Some(json!({"d": 2}));
		result.structured_content = Some(json!({"k": 1}));
		result
	}
}

#[tokio::test]
async fn after_tool_call_overrides_fields_field_by_field() {
	let (agent, _) = scripted_agent(
		vec![
			(
				StopReason::ToolUse,
				vec![ScriptBlock::ToolCall(tool_call("t1", "ls"))],
			),
			text_turn("recovered"),
		],
		vec![Arc::new(FailingTool)],
	);
	agent.set_after_tool_call(Arc::new(|_| AfterToolCallResult {
		content: Some(vec![TextOrImage::text("recovered")]),
		is_error: Some(false),
		..Default::default()
	}));
	let mut events = agent.subscribe();
	agent.prompt("run".into()).await.unwrap();

	let snapshot = agent.snapshot();
	assert_eq!(tool_result_texts(&snapshot), ["recovered"]);
	let Message::ToolResult(result) = snapshot
		.messages
		.iter()
		.find(|message| matches!(message, Message::ToolResult(_)))
		.unwrap()
	else {
		panic!("expected tool result")
	};
	assert!(!result.is_error);
	// Overridden content without structured content drops the structured part;
	// untouched fields keep their executed values.
	assert!(result.details.is_some());
	// The end event reports the merged result without an error flag and, since
	// the override replaced content without structuredContent, without the
	// structured part.
	let mut ends = Vec::new();
	while let Ok(event) = events.try_recv() {
		if let AgentEvent::ToolExecutionEnd {
			tool_name,
			is_error,
			duration_ms,
			result,
			..
		} = event
		{
			ends.push((
				tool_name,
				is_error,
				duration_ms.is_some(),
				result.get("structuredContent").is_some(),
			));
		}
	}
	assert!(
		matches!(ends.as_slice(), [(name, false, true, false)] if name == "ls"),
		"unexpected end event: {ends:?}"
	);
}

#[tokio::test]
async fn after_tool_call_terminate_ends_the_run() {
	let log = execution_log();
	let (agent, _) = scripted_agent(
		vec![(
			StopReason::ToolUse,
			vec![ScriptBlock::ToolCall(tool_call("t1", "ls"))],
		)],
		vec![gated_tool("ls", &log, None, None)],
	);
	agent.set_after_tool_call(Arc::new(|_| AfterToolCallResult {
		terminate: Some(true),
		..Default::default()
	}));
	agent.prompt("run".into()).await.unwrap();

	assert_eq!(*log.lock().unwrap(), ["ls"]);
	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "toolResult"]
	);
}

// --- finish_turn hook --------------------------------------------------------

#[tokio::test]
async fn finish_turn_continue_forces_one_more_request() {
	let (agent, requests) = scripted_agent(
		vec![
			(StopReason::Stop, vec![ScriptBlock::Text("first".into())]),
			(StopReason::Stop, vec![ScriptBlock::Text("second".into())]),
		],
		vec![],
	);
	let calls = Arc::new(AtomicUsize::new(0));
	let hook_calls = calls.clone();
	agent.set_finish_turn(Arc::new(move |_| {
		let hook_calls = hook_calls.clone();
		Box::pin(async move {
			if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
				Some(AgentTurnDecision::Continue)
			} else {
				None
			}
		}) as Pin<Box<dyn Future<Output = Option<AgentTurnDecision>> + Send>>
	}));
	agent.prompt("run".into()).await.unwrap();

	assert_eq!(calls.load(Ordering::SeqCst), 2);
	assert_eq!(
		requests.lock().unwrap().len(),
		2,
		"Continue guarantees a request without queues or tool calls"
	);
	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "assistant"]
	);
}

#[tokio::test]
async fn finish_turn_end_forces_stop_even_with_queued_follow_up() {
	let (agent, _) = scripted_agent(
		vec![(StopReason::Stop, vec![ScriptBlock::Text("only".into())])],
		vec![],
	);
	agent.follow_up("never consumed");
	agent.set_finish_turn(Arc::new(|_| {
		Box::pin(async { Some(AgentTurnDecision::End) })
			as Pin<Box<dyn Future<Output = Option<AgentTurnDecision>> + Send>>
	}));
	agent.prompt("run".into()).await.unwrap();

	assert_eq!(roles_of(&agent.snapshot()), ["system", "user", "assistant"]);
	assert!(
		agent.has_queued_messages(),
		"End must not drain the follow-up queue"
	);
	assert_eq!(agent.peek_follow_up_queue().len(), 1);
}

// --- prepare_request hook ----------------------------------------------------

#[tokio::test]
async fn prepare_request_swaps_model_and_thinking_for_subsequent_requests() {
	let model_b = Model::new("scripted-b", "scripted-b", "scripted-b", "scripted-b", "");
	let turns = vec![
		(StopReason::Stop, vec![ScriptBlock::Text("one".into())]),
		(StopReason::Stop, vec![ScriptBlock::Text("two".into())]),
	];
	let log: RequestLog = Arc::default();
	let agent = Agent::new(AgentOptions {
		model: Model::new("scripted", "scripted", "scripted", "scripted", ""),
		thinking_level: ThinkingLevel::Minimal,
		stream_fn: Some(scripted_stream_fn(turns, log.clone())),
		..Default::default()
	});
	let swapped = model_b.clone();
	let calls = Arc::new(AtomicUsize::new(0));
	let hook_calls = calls.clone();
	agent.set_prepare_request(Arc::new(move |request| {
		let swapped = swapped.clone();
		let hook_calls = hook_calls.clone();
		Box::pin(async move {
			if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
				// First request: the runtime state before the swap.
				assert_eq!(request.model.id, "scripted");
				assert_eq!(request.thinking_level, ThinkingLevel::Minimal);
				assert!(!request.messages.is_empty());
				Some(AgentRequestUpdate {
					model: Some(swapped),
					thinking_level: Some(ThinkingLevel::High),
				})
			} else {
				None
			}
		}) as Pin<Box<dyn Future<Output = Option<AgentRequestUpdate>> + Send>>
	}));
	agent.follow_up("second question");
	agent.prompt("first".into()).await.unwrap();

	// The swap applies to the request being prepared (upstream applies
	// requestUpdate before streaming), so both requests use model B/High; the
	// hook itself observed the pre-swap state on the first call.
	assert_eq!(
		*log.lock().unwrap(),
		vec![
			("scripted-b".to_string(), Some(ThinkingLevel::High)),
			("scripted-b".to_string(), Some(ThinkingLevel::High)),
		]
	);
	assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// --- tool detection + length -------------------------------------------------

#[tokio::test]
async fn tool_calls_execute_even_when_stop_reason_is_stop() {
	let log = execution_log();
	let (agent, _) = scripted_agent(
		vec![
			// Upstream detects tool calls by content, not stop reason.
			(
				StopReason::Stop,
				vec![ScriptBlock::ToolCall(tool_call("t1", "ls"))],
			),
			text_turn("done"),
		],
		vec![gated_tool("ls", &log, None, None)],
	);
	agent.prompt("run".into()).await.unwrap();

	assert_eq!(*log.lock().unwrap(), ["ls"]);
	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "toolResult", "assistant"]
	);
	let snapshot = agent.snapshot();
	let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
		panic!("expected assistant")
	};
	assert!(
		reply.duration_ms.is_some(),
		"the loop stamps duration_ms on the final assistant message"
	);
}

#[tokio::test]
async fn length_stop_fails_the_whole_tool_batch_without_executing() {
	let log = execution_log();
	let (agent, _) = scripted_agent(
		vec![
			(
				StopReason::Length,
				vec![
					ScriptBlock::ToolCall(tool_call("a1", "alpha")),
					ScriptBlock::ToolCall(tool_call("b1", "beta")),
				],
			),
			text_turn("retrying"),
		],
		vec![
			gated_tool("alpha", &log, None, None),
			gated_tool("beta", &log, None, None),
		],
	);
	let mut events = agent.subscribe();
	agent.prompt("run".into()).await.unwrap();

	assert!(
		log.lock().unwrap().is_empty(),
		"truncated tool calls must not execute"
	);
	let texts = tool_result_texts(&agent.snapshot());
	assert_eq!(texts.len(), 2);
	for (name, text) in [("alpha", &texts[0]), ("beta", &texts[1])] {
		assert!(text.contains(name), "error names the tool: {text}");
		assert!(
			text.contains("output token limit") && text.contains("Re-issue"),
			"error carries the upstream truncation semantics: {text}"
		);
	}
	assert!(matches!(
		collect_tool_events(&mut events).as_slice(),
		[
			ToolEvent::Start(_),
			ToolEvent::End(_, true, None),
			ToolEvent::Start(_),
			ToolEvent::End(_, true, None),
		]
	));
	let roles: Vec<&str> = agent
		.snapshot()
		.messages
		.iter()
		.map(Message::role)
		.collect();
	assert_eq!(
		roles,
		[
			"system",
			"user",
			"assistant",
			"toolResult",
			"toolResult",
			"assistant"
		],
		"the error results are appended and the loop continues"
	);
}

// --- set_tools announcement --------------------------------------------------

struct ReadTool {
	description: &'static str,
}

#[async_trait]
impl AgentTool for ReadTool {
	fn name(&self) -> &str {
		"read"
	}
	fn description(&self) -> &str {
		self.description
	}
	fn parameters_schema(&self) -> Value {
		json!({"type": "object"})
	}
	async fn execute(
		&self,
		_: &str,
		_: Value,
		_: CancellationToken,
		_: Option<ToolUpdate>,
	) -> AgentToolResult {
		AgentToolResult::text("")
	}
}

#[tokio::test]
async fn set_tools_announces_added_and_removed_tools() {
	let agent = mock_agent(vec![Arc::new(ListTool)]);
	let mut events = agent.subscribe();
	let read_decl = pi_ai::Tool::new("read", "Read a file", json!({"type": "object"}));

	// Addition: [ls] -> [ls, read]
	agent.set_tools(vec![
		Arc::new(ListTool),
		Arc::new(ReadTool {
			description: "Read a file",
		}),
	]);
	let snapshot = agent.snapshot();
	let Message::System(announcement) = snapshot.messages.last().unwrap() else {
		panic!("expected announcement system message")
	};
	assert_eq!(
		announcement.tools_added.as_deref(),
		Some([read_decl.clone()].as_slice())
	);
	assert!(announcement.tools_removed.is_none());
	assert!(matches!(
		&announcement.content,
		pi_ai::SystemContent::Text(text) if text.is_empty()
	));

	// No-op: same set, no announcement.
	let before = agent.snapshot().messages.len();
	agent.set_tools(vec![
		Arc::new(ListTool),
		Arc::new(ReadTool {
			description: "Read a file",
		}),
	]);
	assert_eq!(agent.snapshot().messages.len(), before);

	// Removal: [ls, read] -> [ls]
	agent.set_tools(vec![Arc::new(ListTool)]);
	let snapshot = agent.snapshot();
	let Message::System(announcement) = snapshot.messages.last().unwrap() else {
		panic!("expected announcement system message")
	};
	assert!(announcement.tools_added.is_none());
	assert_eq!(
		announcement.tools_removed.as_deref(),
		Some(
			[pi_ai::ToolReference {
				name: "read".into()
			}]
			.as_slice()
		)
	);

	// Changed definition = removal + addition. The transcript currently
	// declares [ls], so switching to read-v2 removes ls and adds read.
	agent.set_tools(vec![Arc::new(ReadTool {
		description: "Read a file, v2",
	})]);
	let snapshot = agent.snapshot();
	let Message::System(announcement) = snapshot.messages.last().unwrap() else {
		panic!("expected announcement system message")
	};
	assert_eq!(
		announcement.tools_added.as_deref(),
		Some(
			[pi_ai::Tool::new(
				"read",
				"Read a file, v2",
				json!({"type": "object"})
			)]
			.as_slice()
		)
	);
	assert_eq!(
		announcement.tools_removed.as_deref(),
		Some([pi_ai::ToolReference { name: "ls".into() }].as_slice())
	);

	// Every announcement travels with its lifecycle events.
	let mut system_starts = 0;
	while let Ok(event) = events.try_recv() {
		if let AgentEvent::MessageStart { message } = event {
			assert!(matches!(message, Message::System(_)));
			system_starts += 1;
		}
	}
	assert_eq!(system_starts, 3);
}

// --- queues ------------------------------------------------------------------

#[tokio::test]
async fn per_queue_peek_and_clear_work_independently() {
	let agent = mock_agent(vec![]);
	agent.steer("s1");
	agent.steer("s2");
	agent.follow_up("f1");
	agent.follow_up("f2");

	// Default queue mode is one-at-a-time.
	assert_eq!(roles_of_messages(&agent.peek_steering_queue()), ["user"]);
	assert_eq!(agent.peek_steering_queue().len(), 1);
	assert_eq!(agent.peek_follow_up_queue().len(), 1);
	assert_eq!(agent.peek_queued_messages().len(), 1);

	agent.clear_steering_queue();
	assert!(agent.peek_steering_queue().is_empty());
	assert_eq!(agent.peek_follow_up_queue().len(), 1);
	assert!(agent.has_queued_messages());

	agent.clear_follow_up_queue();
	assert!(agent.peek_follow_up_queue().is_empty());
	assert!(!agent.has_queued_messages());
}

fn roles_of_messages(messages: &[Message]) -> Vec<&str> {
	messages.iter().map(Message::role).collect()
}

#[tokio::test]
async fn steering_mode_all_drains_every_queued_message_at_once() {
	let agent = Agent::new(AgentOptions {
		stream_fn: Some(Arc::new(pi_ai::providers::mock::stream)),
		steering_mode: QueueMode::All,
		..Default::default()
	});
	agent.steer("s1");
	agent.steer("s2");
	agent.prompt("first".into()).await.unwrap();
	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "user", "user", "assistant"]
	);
	assert!(!agent.has_queued_messages());
}

#[tokio::test]
async fn steering_mode_one_at_a_time_drains_one_message_per_turn() {
	let agent = mock_agent(vec![]);
	agent.steer("s1");
	agent.steer("s2");
	agent.prompt("first".into()).await.unwrap();
	assert_eq!(
		roles_of(&agent.snapshot()),
		[
			"system",
			"user",
			"assistant",
			"user",
			"assistant",
			"user",
			"assistant"
		]
	);
	assert!(!agent.has_queued_messages());
}

#[tokio::test]
async fn steering_messages_inject_after_the_current_assistant_turn() {
	let agent = mock_agent(vec![]);
	let mut events = agent.subscribe();
	agent.steer("steered");
	agent.prompt("first".into()).await.unwrap();

	assert_eq!(
		roles_of(&agent.snapshot()),
		["system", "user", "assistant", "user", "assistant"]
	);
	// The steering user message is appended with its lifecycle events after
	// the assistant turn it steers.
	let mut saw_steering_after_assistant = false;
	let mut last_was_assistant_end = false;
	while let Ok(event) = events.try_recv() {
		if let AgentEvent::MessageEnd { message } = event {
			match message {
				Message::Assistant(_) => last_was_assistant_end = true,
				Message::User(_) if last_was_assistant_end => saw_steering_after_assistant = true,
				_ => last_was_assistant_end = false,
			}
		}
	}
	assert!(saw_steering_after_assistant);
}
