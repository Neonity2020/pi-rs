//! Streaming protocol — port of `pi/packages/ai/src/utils/event-stream.ts` and
//! the `StreamFn` contract from `pi/packages/agent/src/types.ts`.
//!
//! Contract (as in the TS pi):
//! - The loop passes a normalized transcript: the system prompt and tool
//!   declarations live in the transcript's system messages, never on the
//!   context itself.
//! - Stream functions must not panic for request/model/runtime failures;
//!   failures are encoded as a final `Error` event carrying an assistant
//!   message with stopReason `error`/`aborted` and `errorMessage`.
//! - Successful streams emit `Start` before partial updates and terminate
//!   with `Done` (or `Error`).

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::model::Model;
use crate::types::{
	AssistantMessage, Message, StopReason, SystemMessage, ThinkingLevel, Tool, TextContent,
	ToolCall,
};

/// Normalized request context passed to providers. Only [`normalize_context`]
/// produces this, so a raw message list cannot reach provider code by accident.
#[derive(Debug, Clone)]
pub struct TranscriptContext {
	pub messages: Vec<Message>,
}

/// Folds the `system_prompt` + `tools` shorthand into a leading system message,
/// mirroring `normalizeContext()`. If the transcript already begins with a
/// system message it is replaced.
pub fn normalize_context(system_prompt: Option<&str>, tools: &[Tool], messages: Vec<Message>) -> TranscriptContext {
	let mut leading = SystemMessage::prompt(system_prompt.unwrap_or(""));
	leading.tools_added = if tools.is_empty() { None } else { Some(tools.to_vec()) };

	let mut out = Vec::with_capacity(messages.len() + 1);
	out.push(Message::System(leading));
	for message in messages {
		if matches!(message, Message::System(_)) {
			// Only one leading system message survives normalization.
			continue;
		}
		out.push(message);
	}
	TranscriptContext { messages: out }
}

/// Event protocol for [`AssistantMessageEventStream`].
///
/// `partial` carries the live response-so-far on every event. Text and
/// thinking blocks are empty at their `*_start` and grow through `*_delta`
/// until the authoritative `*_end`. Tool-call arguments at `ToolCallStart`
/// are provider-specific; `ToolCallDelta` carries subsequent JSON updates.
#[derive(Debug, Clone)]
pub enum AssistantMessageEvent {
	Start {
		partial: AssistantMessage,
	},
	TextStart {
		content_index: usize,
		partial: AssistantMessage,
	},
	TextDelta {
		content_index: usize,
		delta: String,
		partial: AssistantMessage,
	},
	TextEnd {
		content_index: usize,
		content: String,
		partial: AssistantMessage,
	},
	ThinkingStart {
		content_index: usize,
		partial: AssistantMessage,
	},
	ThinkingDelta {
		content_index: usize,
		delta: String,
		partial: AssistantMessage,
	},
	ThinkingEnd {
		content_index: usize,
		partial: AssistantMessage,
	},
	ToolCallStart {
		content_index: usize,
		partial: AssistantMessage,
	},
	ToolCallDelta {
		content_index: usize,
		delta: String,
		partial: AssistantMessage,
	},
	ToolCallEnd {
		content_index: usize,
		tool_call: ToolCall,
		partial: AssistantMessage,
	},
	Done {
		reason: StopReason,
		message: AssistantMessage,
	},
	Error {
		reason: StopReason,
		error: AssistantMessage,
	},
}

impl AssistantMessageEvent {
	/// The live partial (or final/error) assistant message carried by the event.
	pub fn partial(&self) -> &AssistantMessage {
		match self {
			AssistantMessageEvent::Start { partial }
			| AssistantMessageEvent::TextStart { partial, .. }
			| AssistantMessageEvent::TextDelta { partial, .. }
			| AssistantMessageEvent::TextEnd { partial, .. }
			| AssistantMessageEvent::ThinkingStart { partial, .. }
			| AssistantMessageEvent::ThinkingDelta { partial, .. }
			| AssistantMessageEvent::ThinkingEnd { partial, .. }
			| AssistantMessageEvent::ToolCallStart { partial, .. }
			| AssistantMessageEvent::ToolCallDelta { partial, .. }
			| AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
			AssistantMessageEvent::Done { message, .. } => message,
			AssistantMessageEvent::Error { error, .. } => error,
		}
	}
}

/// Boxed async stream of assistant message events.
pub type AssistantMessageEventStream = Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send>>;

/// Options shared by stream calls — subset of the TS pi's `SimpleStreamOptions`
/// relevant to the loop. Provider-specific fields are added per adapter.
#[derive(Clone)]
pub struct StreamOptions {
	pub api_key: Option<String>,
	pub temperature: Option<f64>,
	pub max_tokens: Option<u64>,
	/// Provider-neutral reasoning request; `None` maps to `ThinkingLevel::Off`.
	pub reasoning: Option<ThinkingLevel>,
	/// Cooperative cancellation for the in-flight provider request.
	pub signal: CancellationToken,
	/// Optional session identifier for cache-aware providers.
	pub session_id: Option<String>,
}

impl Default for StreamOptions {
	fn default() -> Self {
		Self {
			api_key: None,
			temperature: None,
			max_tokens: None,
			reasoning: None,
			signal: CancellationToken::new(),
			session_id: None,
		}
	}
}

/// Stream function used by the agent loop; `Models.streamSimple` in the TS pi
/// satisfies the same shape.
pub type StreamFn = Arc<
	dyn Fn(&Model, &TranscriptContext, StreamOptions) -> AssistantMessageEventStream + Send + Sync,
>;

/// Helper for building streaming text responses event by event.
pub struct TextStreamBuilder {
	message: AssistantMessage,
	text: String,
}

impl TextStreamBuilder {
	pub fn new(message: AssistantMessage) -> Self {
		Self { message, text: String::new() }
	}

	pub fn start(mut self) -> Self {
		self.message.content.push(crate::types::AssistantContent::Text(TextContent {
			text: String::new(),
			text_signature: None,
		}));
		self
	}

	pub fn delta(&mut self, delta: impl Into<String>) -> AssistantMessageEvent {
		let delta = delta.into();
		self.text.push_str(&delta);
		self.patch_tail();
		AssistantMessageEvent::TextDelta {
			content_index: self.message.content.len() - 1,
			delta,
			partial: self.message.clone(),
		}
	}

	pub fn done(self, mut message: AssistantMessage) -> (AssistantMessageEvent, AssistantMessage) {
		message.content = self.message.content.clone();
		message.stop_reason = StopReason::Stop;
		let event = AssistantMessageEvent::Done {
			reason: StopReason::Stop,
			message: message.clone(),
		};
		(event, message)
	}

	fn patch_tail(&mut self) {
		if let Some(crate::types::AssistantContent::Text(t)) = self.message.content.last_mut() {
			t.text = self.text.clone();
		}
	}
}
