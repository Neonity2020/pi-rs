//! Mock adapter — a complete, protocol-correct in-process provider.
//!
//! Used as the fallback stream function until the HTTP adapters land, and for
//! tests. Behavior:
//! - a user message mentioning "list"/"ls"/"目录" triggers an `ls` tool call
//!   (demonstrating the full agent loop: toolUse → execution → follow-up turn);
//! - after a tool result, the next turn summarizes the result;
//! - anything else echoes the input with simulated streaming deltas.

use std::time::Duration;

use serde_json::json;

use crate::model::Model;
use crate::stream::{AssistantMessageEvent, StreamOptions, TranscriptContext};
use crate::types::{
	now_ms, AssistantContent, AssistantMessage, StopReason, TextContent, ToolCall, Usage,
};

const CHUNK_CHARS: usize = 24;
const DELTA_DELAY_MS: u64 = 12;

pub fn stream(
	model: &Model,
	context: &TranscriptContext,
	options: StreamOptions,
) -> crate::stream::AssistantMessageEventStream {
	let events = plan_response(model, context);
	let signal = options.signal.clone();
	let cancelled_reason = StopReason::Aborted;
	let model = model.clone();

	let stream = futures::stream::unfold((events, 0usize), move |(events, mut i)| {
		let signal = signal.clone();
		let model = model.clone();
		async move {
			if signal.is_cancelled() {
				// Emit a single abort event, then terminate.
				if i != usize::MAX {
					let error = AssistantMessage::error(&model, cancelled_reason, "aborted by user");
					return Some((
						AssistantMessageEvent::Error { reason: cancelled_reason, error },
						(events, usize::MAX),
					));
				}
				return None;
			}
			if i == usize::MAX || i >= events.len() {
				return None;
			}
			let event = events[i].clone();
			i += 1;
			if matches!(
				event,
				AssistantMessageEvent::TextDelta { .. } | AssistantMessageEvent::ThinkingDelta { .. }
			) {
				tokio::time::sleep(Duration::from_millis(DELTA_DELAY_MS)).await;
			}
			Some((event, (events, i)))
		}
	});
	Box::pin(stream)
}

fn plan_response(model: &Model, context: &TranscriptContext) -> Vec<AssistantMessageEvent> {
	let last = context.messages.last();
	match last {
		Some(crate::types::Message::ToolResult(result)) => {
			let text = result.text();
			let preview = truncate_chars(&text, 600);
			let reply = format!("[mock provider] The `{}` tool returned:\n\n{preview}", result.tool_name);
			let usage = fake_usage(reply.len());
			let mut message = base_message(model, StopReason::Stop, usage);
			text_events(&mut message, &reply)
		}
		Some(crate::types::Message::User(user)) => {
			let text = user.content.as_text();
			let lower = text.to_lowercase();
			if lower.contains("ls") || lower.contains("list") || text.contains("目录") || text.contains("列表") {
				let call = ToolCall {
					id: format!("call-mock-{}", now_ms()),
					name: "ls".to_string(),
					arguments: json!({ "path": "." }),
					thought_signature: None,
					namespace: None,
					arguments_error: None,
				};
				let message = base_message(model, StopReason::ToolUse, Usage::default());
				tool_call_events(message, call)
			} else {
				let reply = format!(
					"[mock provider] You said: {text}\n\nSet OPENAI_API_KEY and select an OpenAI model for real responses.\nTip: ask me to \"list files\" to see a tool call round-trip."
				);
				let usage = fake_usage(reply.len());
				let mut message = base_message(model, StopReason::Stop, usage);
				text_events(&mut message, &reply)
			}
		}
		_ => {
			let mut message = base_message(model, StopReason::Stop, Usage::default());
			text_events(&mut message, "[mock provider] Waiting for a user message.")
		}
	}
}

fn base_message(model: &Model, stop_reason: StopReason, usage: Usage) -> AssistantMessage {
	AssistantMessage {
		content: Vec::new(),
		api: model.api.as_str().to_string(),
		provider: model.provider.clone(),
		model: model.id.clone(),
		response_model: None,
		response_id: None,
		thinking_level: None,
		usage,
		stop_reason,
		error_message: None,
		raw_stop_reason: None,
		end_turn: None,
		timestamp: now_ms(),
		duration_ms: None,
	}
}

fn truncate_chars(text: &str, max: usize) -> String {
	if text.chars().count() <= max {
		return text.to_string();
	}
	let mut out: String = text.chars().take(max).collect();
	out.push_str("\n…");
	out
}

fn fake_usage(output_len: usize) -> Usage {
	Usage {
		input: 128,
		output: (output_len as u64) / 4,
		cache_read: 0,
		cache_write: 0,
		cache_write_1h: None,
		reasoning: None,
		total_tokens: 128 + (output_len as u64) / 4,
		cost: Default::default(),
	}
}

fn text_events(message: &mut AssistantMessage, text: &str) -> Vec<AssistantMessageEvent> {
	let mut events = Vec::new();
	let mut partial = message.clone();
	partial.content = vec![AssistantContent::Text(TextContent { text: String::new(), text_signature: None })];
	events.push(AssistantMessageEvent::Start { partial: partial.clone() });
	events.push(AssistantMessageEvent::TextStart { content_index: 0, partial: partial.clone() });

	let chars: Vec<char> = text.chars().collect();
	let mut acc = String::new();
	for chunk in chars.chunks(CHUNK_CHARS) {
		let delta: String = chunk.iter().collect();
		acc.push_str(&delta);
		partial.content = vec![AssistantContent::Text(TextContent { text: acc.clone(), text_signature: None })];
		events.push(AssistantMessageEvent::TextDelta {
			content_index: 0,
			delta,
			partial: partial.clone(),
		});
	}

	partial.content = vec![AssistantContent::Text(TextContent { text: text.to_string(), text_signature: None })];
	events.push(AssistantMessageEvent::TextEnd {
		content_index: 0,
		content: text.to_string(),
		partial: partial.clone(),
	});

	let mut final_message = message.clone();
	final_message.content = partial.content.clone();
	events.push(AssistantMessageEvent::Done {
		reason: StopReason::Stop,
		message: final_message,
	});
	events
}

fn tool_call_events(mut message: AssistantMessage, call: ToolCall) -> Vec<AssistantMessageEvent> {
	let mut events = Vec::new();
	let mut partial = message.clone();
	partial.content = vec![AssistantContent::ToolCall(call.clone())];
	events.push(AssistantMessageEvent::Start { partial: partial.clone() });
	events.push(AssistantMessageEvent::ToolCallStart { content_index: 0, partial: partial.clone() });
	events.push(AssistantMessageEvent::ToolCallEnd {
		content_index: 0,
		tool_call: call,
		partial: partial.clone(),
	});
	message.content = partial.content;
	events.push(AssistantMessageEvent::Done {
		reason: StopReason::ToolUse,
		message,
	});
	events
}
