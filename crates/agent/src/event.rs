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
