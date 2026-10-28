//! Normalized message model — port of `pi/packages/ai/src/types.ts`.
//!
//! All structs serialize camelCase with the same discriminants as the TS pi
//! (`role` tag on messages, `type` tag on content blocks), so JSON session
//! transcripts and RPC payloads are cross-compatible.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type JsonValue = Value;

// ---------------------------------------------------------------------------
// Thinking levels
// ---------------------------------------------------------------------------

/// Thinking/reasoning level for models that support it.
/// "xhigh" and "max" are only supported by selected model families; consult
/// the model's thinking level map from the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
	#[default]
	Off,
	Minimal,
	Low,
	Medium,
	High,
	Xhigh,
	Max,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
	pub text: String,
	/// e.g. for OpenAI responses: message metadata (legacy id or signature JSON).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub text_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
	pub thinking: String,
	/// Provider-specific opaque reasoning replay data.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub thinking_signature: Option<String>,
	/// True when the thinking content was redacted by safety filters.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub redacted: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
	/// Base64-encoded image data.
	pub data: String,
	/// e.g. "image/png".
	pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
	pub id: String,
	pub name: String,
	pub arguments: Value,
	/// Google-specific: opaque signature for reusing thought context.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub thought_signature: Option<String>,
	/// OpenAI Responses namespace for namespaced tools.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub namespace: Option<String>,
	/// Set when the model's complete arguments were not valid JSON.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub arguments_error: Option<String>,
}

/// Content blocks allowed inside an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
	Text(TextContent),
	Thinking(ThinkingContent),
	ToolCall(ToolCall),
}

/// Content blocks allowed in user messages and tool results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TextOrImage {
	Text(TextContent),
	Image(ImageContent),
}

impl TextOrImage {
	pub fn text(s: impl Into<String>) -> Self {
		TextOrImage::Text(TextContent {
			text: s.into(),
			text_signature: None,
		})
	}

	pub fn text_content(&self) -> Option<&str> {
		match self {
			TextOrImage::Text(t) => Some(&t.text),
			TextOrImage::Image(_) => None,
		}
	}
}

/// System message content: plain string or text blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SystemContent {
	Text(String),
	Blocks(Vec<TextContent>),
}

/// User message content: plain string or text/image blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UserContent {
	Text(String),
	Blocks(Vec<TextOrImage>),
}

impl UserContent {
	pub fn text(s: impl Into<String>) -> Self {
		UserContent::Text(s.into())
	}

	/// Concatenated text of all text blocks (images are skipped).
	pub fn as_text(&self) -> String {
		match self {
			UserContent::Text(s) => s.clone(),
			UserContent::Blocks(blocks) => blocks
				.iter()
				.filter_map(|b| b.text_content())
				.collect::<Vec<_>>()
				.join("\n"),
		}
	}
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
	pub input: f64,
	pub output: f64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cache_read: Option<f64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cache_write: Option<f64>,
	/// Best available total: provider-reported billed cost when supported,
	/// otherwise the catalog estimate.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub total: Option<f64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub total_from_provider: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
	#[serde(default)]
	pub input: u64,
	#[serde(default)]
	pub output: u64,
	#[serde(default)]
	pub cache_read: u64,
	#[serde(default)]
	pub cache_write: u64,
	/// Subset of `cache_write` written with 1h retention (Anthropic only).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cache_write_1h: Option<u64>,
	/// Reasoning tokens, a subset of `output`, when reported.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reasoning: Option<u64>,
	#[serde(default)]
	pub total_tokens: u64,
	#[serde(default)]
	pub cost: Cost,
}

impl Usage {
	/// Total context tokens consumed by the request (`calculateContextTokens`
	/// upstream, `pi/packages/ai` compaction accounting): the provider-reported
	/// `total_tokens` when positive, otherwise the sum of the components.
	pub fn context_tokens(&self) -> u64 {
		if self.total_tokens > 0 {
			self.total_tokens
		} else {
			self.input
				.saturating_add(self.output)
				.saturating_add(self.cache_read)
				.saturating_add(self.cache_write)
		}
	}
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
	#[default]
	Pending,
	Stop,
	Length,
	ToolUse,
	Error,
	Aborted,
	Deferred,
}

/// Tool declaration as carried inside system messages and requests.
///
/// `parameters` is a JSON Schema object (the TS pi uses TypeBox `TSchema`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
	pub name: String,
	pub description: String,
	pub parameters: JsonValue,
}

impl Tool {
	pub fn new(
		name: impl Into<String>,
		description: impl Into<String>,
		parameters: JsonValue,
	) -> Self {
		Self {
			name: name.into(),
			description: description.into(),
			parameters,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolReference {
	pub name: String,
}

/// System instructions and tool declarations at one point in the transcript.
///
/// The leading system message is the system prompt. Later system messages
/// change it: `content` adds instructions from that point on, `sections`
/// replace or remove named prompt sections, and `tools_added`/`tools_removed`
/// change the tool set. Replaying every system message in order yields the
/// current prompt and tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
	pub content: SystemContent,
	/// Named, ordered prompt sections rendered after `content`; `None` removes.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub sections: Option<BTreeMap<String, Option<String>>>,
	/// Complete definitions of tools that become available at this point.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tools_added: Option<Vec<Tool>>,
	/// Tools that stop being available at this point.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tools_removed: Option<Vec<ToolReference>>,
	/// Unix timestamp in milliseconds.
	pub timestamp: u64,
}

impl SystemMessage {
	pub fn prompt(text: impl Into<String>) -> Self {
		Self {
			content: SystemContent::Text(text.into()),
			sections: None,
			tools_added: None,
			tools_removed: None,
			timestamp: now_ms(),
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
	pub content: UserContent,
	/// Unix timestamp in milliseconds.
	pub timestamp: u64,
}

impl UserMessage {
	pub fn text(s: impl Into<String>) -> Self {
		Self {
			content: UserContent::text(s),
			timestamp: now_ms(),
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
	pub content: Vec<AssistantContent>,
	pub api: String,
	pub provider: String,
	pub model: String,
	/// Concrete model reported by the provider, when reported.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub response_model: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub response_id: Option<String>,
	/// Pi thinking level the agent loop requested for this response.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub thinking_level: Option<ThinkingLevel>,
	#[serde(default)]
	pub usage: Usage,
	pub stop_reason: StopReason,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error_message: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub raw_stop_reason: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub end_turn: Option<bool>,
	/// Unix timestamp in milliseconds when the request started.
	pub timestamp: u64,
	/// Milliseconds from `timestamp` until the response ended.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub duration_ms: Option<u64>,
}

impl AssistantMessage {
	/// Empty assistant message carrying an error/abort stop reason.
	pub fn error(
		model: &crate::model::Model,
		stop_reason: StopReason,
		message: impl Into<String>,
	) -> Self {
		Self {
			content: vec![AssistantContent::Text(TextContent {
				text: String::new(),
				text_signature: None,
			})],
			api: model.api.to_string(),
			provider: model.provider.clone(),
			model: model.id.clone(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: Usage::default(),
			stop_reason,
			error_message: Some(message.into()),
			raw_stop_reason: None,
			end_turn: None,
			timestamp: now_ms(),
			duration_ms: None,
		}
	}

	/// Concatenated text of all text blocks.
	pub fn text(&self) -> String {
		self.content
			.iter()
			.filter_map(|c| match c {
				AssistantContent::Text(t) => Some(t.text.as_str()),
				_ => None,
			})
			.collect::<Vec<_>>()
			.join("")
	}

	pub fn thinking(&self) -> String {
		self.content
			.iter()
			.filter_map(|c| match c {
				AssistantContent::Thinking(t) => Some(t.thinking.as_str()),
				_ => None,
			})
			.collect::<Vec<_>>()
			.join("")
	}

	pub fn tool_calls(&self) -> Vec<&ToolCall> {
		self.content
			.iter()
			.filter_map(|c| match c {
				AssistantContent::ToolCall(tc) => Some(tc),
				_ => None,
			})
			.collect()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
	pub tool_call_id: String,
	pub tool_name: String,
	pub content: Vec<TextOrImage>,
	/// Arbitrary structured details for logs or UI rendering.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub details: Option<JsonValue>,
	/// Usage from the tool execution itself. Not part of main LLM accounting.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub usage: Option<Usage>,
	#[serde(default)]
	pub is_error: bool,
	/// Unix timestamp in milliseconds when the result was created.
	pub timestamp: u64,
	/// Milliseconds the tool's execution took.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub duration_ms: Option<u64>,
}

impl ToolResultMessage {
	pub fn text_result(
		tool_call_id: impl Into<String>,
		tool_name: impl Into<String>,
		text: impl Into<String>,
		is_error: bool,
	) -> Self {
		Self {
			tool_call_id: tool_call_id.into(),
			tool_name: tool_name.into(),
			content: vec![TextOrImage::text(text)],
			details: None,
			usage: None,
			is_error,
			timestamp: now_ms(),
			duration_ms: None,
		}
	}

	/// Concatenated text of the result content.
	pub fn text(&self) -> String {
		self.content
			.iter()
			.filter_map(|b| b.text_content())
			.collect::<Vec<_>>()
			.join("\n")
	}
}

/// The normalized LLM message union.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
	System(SystemMessage),
	User(UserMessage),
	Assistant(AssistantMessage),
	ToolResult(ToolResultMessage),
}

impl Message {
	pub fn system(content: impl Into<String>) -> Self {
		Message::System(SystemMessage::prompt(content))
	}

	pub fn user(content: impl Into<String>) -> Self {
		Message::User(UserMessage::text(content))
	}

	pub fn role(&self) -> &'static str {
		match self {
			Message::System(_) => "system",
			Message::User(_) => "user",
			Message::Assistant(_) => "assistant",
			Message::ToolResult(_) => "toolResult",
		}
	}

	pub fn timestamp(&self) -> u64 {
		match self {
			Message::System(m) => m.timestamp,
			Message::User(m) => m.timestamp,
			Message::Assistant(m) => m.timestamp,
			Message::ToolResult(m) => m.timestamp,
		}
	}
}

pub fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn context_tokens_prefers_provider_total() {
		let usage = Usage {
			input: 100,
			output: 20,
			cache_read: 30,
			cache_write: 40,
			total_tokens: 250,
			..Default::default()
		};
		assert_eq!(usage.context_tokens(), 250);
	}

	#[test]
	fn context_tokens_falls_back_to_components() {
		let usage = Usage {
			input: 100,
			output: 20,
			cache_read: 30,
			cache_write: 40,
			total_tokens: 0,
			..Default::default()
		};
		assert_eq!(usage.context_tokens(), 190);
	}

	#[test]
	fn context_tokens_of_default_usage_is_zero() {
		assert_eq!(Usage::default().context_tokens(), 0);
	}
}
