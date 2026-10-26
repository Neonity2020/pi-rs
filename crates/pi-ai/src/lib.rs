//! pi-ai — model-agnostic AI layer for pi-rs.
//!
//! Rust port of `pi/packages/ai`: the normalized message model
//! (`SystemMessage` / `UserMessage` / `AssistantMessage` / `ToolResultMessage`),
//! the `AssistantMessageEvent` streaming protocol, the model catalog, and
//! provider API adapters (`anthropic-messages`, `openai-completions`,
//! `openai-responses`, `google-generative-ai`, ...).
//!
//! The wire JSON is camelCase, matching the TS pi exactly so session files,
//! model catalogs, and RPC payloads stay interchangeable.

pub mod model;
pub mod providers;
pub mod stream;
pub mod types;

pub use model::{Api, InputModality, Model, ModelCost};
pub use stream::{
	normalize_context, AssistantMessageEvent, AssistantMessageEventStream, StreamFn, StreamOptions,
	TranscriptContext,
};
pub use types::{
	AssistantContent, AssistantMessage, Cost, ImageContent, JsonValue, Message, StopReason,
	SystemContent, SystemMessage, TextContent, TextOrImage, ThinkingContent, ThinkingLevel,
	Tool, ToolCall, ToolReference, ToolResultMessage, Usage, UserContent, UserMessage,
};

/// Crate-wide error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("provider adapter not implemented yet: {0}")]
	UnsupportedAdapter(String),
	#[error("missing API key for provider {0}")]
	MissingApiKey(String),
	#[error("request failed: {0}")]
	Request(String),
	#[error(transparent)]
	Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
