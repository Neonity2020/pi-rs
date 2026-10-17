//! pi-agent — the agent loop of pi-rs.
//!
//! One job: given a provider, a model, a system prompt, a tool set and a
//! message history, run turns until the model produces a response without
//! tool calls. It emits an event stream and persists nothing — sessions,
//! prompts and terminals belong to the `coding` layer.
//!
//! No max-steps knob: the loop loops until the agent is done, until it is
//! cancelled, or until it fails.

mod agent;
mod compaction;
mod event;
mod tool;

pub use llm;

pub use agent::{Agent, Compaction, CompactionMode, Outcome};
pub use compaction::{
    estimate_context_tokens, estimate_messages_tokens, estimate_tokens, find_cut_index,
    is_context_overflow, is_summary_message, summary_message, summary_prompt,
};
pub use event::{AgentEvent, Sink};
pub use tool::{Tool, ToolResult};

pub use llm::{
    Cancel, ChatRequest, Error, Image, Message, Model, Provider, Role, StopReason, StreamEvent,
    ToolCall, ToolSpec, Usage,
};

/// The provider layer is upstream of everything; re-export the boxed-future
/// alias so tool implementations don't need to depend on `pi-llm` directly.
pub use llm::BoxFuture;
