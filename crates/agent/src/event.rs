use serde::Serialize;

use crate::ToolResult;
use crate::{Message, ToolCall, Usage};

/// Everything the UI / session writer needs to know, in order.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// One model request begins (a "step" inside a turn).
    Step {
        step: u32,
    },
    Text {
        delta: String,
    },
    Reasoning {
        delta: String,
    },
    /// A complete assistant message (text and/or tool calls) was committed.
    Assistant {
        message: Message,
    },
    ToolStart {
        call: ToolCall,
    },
    ToolEnd {
        call: ToolCall,
        result: ToolResult,
        duration_ms: u64,
    },
    /// Older messages were summarized. `first_kept_index` points into the
    /// pre-compaction message list; the session layer uses it to record which
    /// entry stays. The summary is carried so the session writer does not
    /// have to recompute it.
    Compaction {
        summary: String,
        tokens_before: u64,
        tokens_after: u64,
        first_kept_index: usize,
    },
    /// Compaction was attempted but failed; the history it would have
    /// replaced is unchanged.
    CompactionFailed {
        message: String,
    },
    Usage {
        usage: Usage,
    },
}

/// Synchronous event consumer, called inline from the agent loop.
pub trait Sink: Send {
    fn on(&mut self, event: AgentEvent);
}

/// Drop-in sink for tests and headless runs that don't care.
impl Sink for () {
    fn on(&mut self, _event: AgentEvent) {}
}
