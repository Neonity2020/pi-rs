//! pi-llm — the provider layer of pi-rs.
//!
//! One job: turn a `ChatRequest` into a stream of `StreamEvent`s.
//! It knows nothing about tools, sessions, prompts, or terminals.
//!
//! Layers: `coding -> agent -> llm`.

pub mod cancel;
pub mod deepseek;
pub mod error;
pub mod message;
pub mod mock;
pub mod model;
pub mod openai;
pub mod provider;

pub use cancel::Cancel;
pub use error::Error;
pub use message::{Message, Role, ToolCall};
pub use model::{Model, ModelCost, Rate, Usage};
pub use openai::OpenAiCompat;
pub use provider::{
    BoxFuture, ChatRequest, Effort, Provider, StopReason, StreamEvent, Thinking, ToolSpec,
};
