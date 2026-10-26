//! Agent-loop types — port of `pi/packages/agent/src/types.ts`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use pi_ai::{
	AssistantContent, AssistantMessage, Message, Model, StopReason, TextOrImage, Tool, ToolCall,
	ToolResultMessage, Usage,
};

/// How tool calls from a single assistant message are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolExecutionMode {
	/// Each tool call is prepared, executed, and finalized before the next.
	Sequential,
	/// Tool calls are prepared sequentially, then allowed tools run concurrently.
	#[default]
	Parallel,
}

/// How many queued messages are injected at a queue drain point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueMode {
	/// Drain and inject every queued message.
	All,
	/// Drain only the oldest queued message.
	#[default]
	OneAtATime,
}

pub const DEFAULT_TOOL_EXECUTION: ToolExecutionMode = ToolExecutionMode::Parallel;

/// Callback used by tools to stream partial execution updates.
pub type ToolUpdate = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// Final or partial result produced by a tool — mirrors `AgentToolResult`.
#[derive(Debug, Clone, Default)]
pub struct AgentToolResult {
	/// Text or image content returned to the model.
	pub content: Vec<TextOrImage>,
	/// Arbitrary structured details for logs or UI rendering.
	pub details: Option<Value>,
	/// Machine-readable result matching the tool's output schema, if any.
	pub structured_content: Option<Value>,
	/// Usage from the tool execution itself, if available.
	pub usage: Option<Usage>,
	/// Report a failure without panicking; the model sees an error result.
	pub is_error: bool,
	/// Hint that the agent should stop after the current tool batch.
	pub terminate: bool,
}

impl AgentToolResult {
	pub fn text(s: impl Into<String>) -> Self {
		Self { content: vec![TextOrImage::text(s)], ..Default::default() }
	}

	pub fn error(s: impl Into<String>) -> Self {
		Self { is_error: true, ..Self::text(s) }
	}
}

/// Tool definition used by the agent runtime — mirrors `AgentTool`.
#[async_trait]
pub trait AgentTool: Send + Sync {
	/// Tool name as exposed to the model.
	fn name(&self) -> &str;

	/// Human-readable label for UI display.
	fn label(&self) -> &str {
		self.name()
	}

	/// Description sent to the model.
	fn description(&self) -> &str;

	/// JSON Schema of the tool's parameters.
	fn parameters_schema(&self) -> Value;

	/// JSON Schema of `structured_content`, when the tool provides it.
	fn output_schema(&self) -> Option<Value> {
		None
	}

	/// Per-tool execution override (`sequential` forces serialized execution).
	fn execution_mode(&self) -> Option<ToolExecutionMode> {
		None
	}

	/// Recovery policy for durable replay (pi-durable phase).
	fn replay(&self) -> &'static str {
		"never"
	}

	/// Execute the tool call. Return `is_error: true` on failure instead of
	/// panicking; honor `signal` for long-running work.
	async fn execute(
		&self,
		tool_call_id: &str,
		params: Value,
		signal: CancellationToken,
		on_update: Option<ToolUpdate>,
	) -> AgentToolResult;

	/// Convert to the wire `Tool` declaration sent inside system messages.
	fn to_declaration(&self) -> Tool {
		Tool::new(self.name(), self.description(), self.parameters_schema())
	}
}

/// Result returned from `before_tool_call` hooks.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
	/// Prevent the tool from executing; the loop emits an error tool result.
	pub block: bool,
	/// Text shown in the blocked error result.
	pub reason: Option<String>,
	/// Hint that the agent should stop after the current tool batch.
	pub terminate: bool,
}

/// Context passed to `before_tool_call` hooks.
pub struct BeforeToolCallContext<'a> {
	pub assistant_message: &'a AssistantMessage,
	pub tool_call: &'a ToolCall,
	/// Current loop context at the time the tool call is prepared.
	pub messages: &'a [Message],
}

/// Partial override returned from `after_tool_call` hooks — field-by-field
/// merge semantics as in the TS pi.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
	pub result: Option<AgentToolResult>,
	pub terminate: Option<bool>,
}

/// Context passed to `after_tool_call` hooks.
pub struct AfterToolCallContext<'a> {
	pub assistant_message: &'a AssistantMessage,
	pub tool_call: &'a ToolCall,
	pub result: &'a AgentToolResult,
	pub is_error: bool,
	pub messages: &'a [Message],
}

/// Decision returned by `finish_turn` hooks; `None` preserves scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTurnDecision {
	Continue,
	End,
}

/// Hook types. The loop invokes them at the same points as the TS pi
/// (`agent-loop.ts`); hooks must not panic.
pub type BeforeToolCallFn =
	Arc<dyn Fn(BeforeToolCallContext<'_>) -> BeforeToolCallResult + Send + Sync>;
pub type AfterToolCallFn = Arc<dyn Fn(AfterToolCallContext<'_>) -> AfterToolCallResult + Send + Sync>;

/// Input accepted by [`crate::Agent::prompt`].
pub enum PromptInput {
	Text { text: String },
	Messages(Vec<Message>),
}

impl From<&str> for PromptInput {
	fn from(s: &str) -> Self {
		PromptInput::Text { text: s.to_string() }
	}
}

impl From<String> for PromptInput {
	fn from(s: String) -> Self {
		PromptInput::Text { text: s }
	}
}

/// Events emitted by the agent for UI updates — mirrors `AgentEvent`.
///
/// Delivered through a tokio broadcast channel; `subscribe()` returns a new
/// receiver. `AgentEnd` is the last event of a run.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
	AgentStart,
	AgentEnd {
		messages: Vec<Message>,
	},
	TurnStart,
	TurnEnd {
		message: Message,
		tool_results: Vec<ToolResultMessage>,
	},
	MessageStart {
		message: Message,
	},
	/// Only emitted for assistant messages during streaming.
	MessageUpdate {
		message: Message,
	},
	MessageEnd {
		message: Message,
	},
	ToolExecutionStart {
		tool_call_id: String,
		tool_name: String,
		args: Value,
	},
	ToolExecutionUpdate {
		tool_call_id: String,
		tool_name: String,
		partial_result: Value,
	},
	ToolExecutionEnd {
		tool_call_id: String,
		tool_name: String,
		result: Value,
		is_error: bool,
		/// Milliseconds `execute()` took; absent when the tool did not run.
		#[serde(skip_serializing_if = "Option::is_none")]
		duration_ms: Option<u64>,
	},
}

impl AgentEvent {
	/// Name of the event, matching the TS pi's `type` strings.
	pub fn kind(&self) -> &'static str {
		match self {
			AgentEvent::AgentStart => "agent_start",
			AgentEvent::AgentEnd { .. } => "agent_end",
			AgentEvent::TurnStart => "turn_start",
			AgentEvent::TurnEnd { .. } => "turn_end",
			AgentEvent::MessageStart { .. } => "message_start",
			AgentEvent::MessageUpdate { .. } => "message_update",
			AgentEvent::MessageEnd { .. } => "message_end",
			AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
			AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
			AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
		}
	}
}

/// Internal queue with `all`/`one-at-a-time` drain semantics — mirrors
/// `PendingMessageQueue`.
#[derive(Debug, Default)]
pub struct PendingMessageQueue {
	pub mode: QueueMode,
	messages: Vec<Message>,
}

impl PendingMessageQueue {
	pub fn new(mode: QueueMode) -> Self {
		Self { mode, messages: Vec::new() }
	}

	pub fn enqueue(&mut self, message: Message) {
		self.messages.push(message);
	}

	pub fn has_items(&self) -> bool {
		!self.messages.is_empty()
	}

	pub fn peek(&self) -> Vec<Message> {
		match self.mode {
			QueueMode::All => self.messages.clone(),
			QueueMode::OneAtATime => self.messages.first().cloned().into_iter().collect(),
		}
	}

	pub fn drain(&mut self) -> Vec<Message> {
		let drained = self.peek();
		self.messages.drain(..drained.len());
		drained
	}

	pub fn clear(&mut self) {
		self.messages.clear();
	}

	pub fn len(&self) -> usize {
		self.messages.len()
	}

	pub fn is_empty(&self) -> bool {
		self.messages.is_empty()
	}
}

/// Empty usage constant mirroring the TS pi's `EMPTY_USAGE`.
pub const EMPTY_USAGE: Usage = Usage {
	input: 0,
	output: 0,
	cache_read: 0,
	cache_write: 0,
	cache_write_1h: None,
	reasoning: None,
	total_tokens: 0,
	cost: pi_ai::Cost {
		input: 0.0,
		output: 0.0,
		cache_read: None,
		cache_write: None,
		total: None,
		total_from_provider: None,
	},
};

/// Unknown-model placeholder mirroring the TS pi's `DEFAULT_MODEL`.
pub fn default_model() -> Model {
	Model::new("unknown", "unknown", pi_ai::Api::new("unknown"), "unknown", "")
}

/// Extract the tool-call blocks from an assistant message.
pub fn assistant_tool_calls(message: &AssistantMessage) -> Vec<&ToolCall> {
	message
		.content
		.iter()
		.filter_map(|c| match c {
			AssistantContent::ToolCall(tc) => Some(tc),
			_ => None,
		})
		.collect()
}

/// True for stop reasons that end the loop regardless of queues/hooks.
pub fn is_hard_exit(reason: StopReason) -> bool {
	matches!(reason, StopReason::Error | StopReason::Aborted)
}
