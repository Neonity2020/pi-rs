//! Context compaction — port of
//! `pi/packages/coding-agent/src/core/compaction/compaction.ts` and
//! `.../compaction/utils.ts` (summarization prompts, conversation
//! serialization, failure checks), plus the compaction-summary message
//! conversion from `pi/packages/coding-agent/src/core/messages.ts`.
//!
//! Reduced to the manual `/compact` path: the whole non-system transcript is
//! summarized by one standalone LLM call through the agent's own
//! model/stream_fn, and the history is replaced with
//! `[system messages…, compaction summary]` — upstream's "keep nothing" cut
//! (`firstKeptEntryId = entry id`).
//!
//! Not ported yet (listed against the upstream source):
//! - automatic threshold/overflow triggering (`shouldCompact`,
//!   `CompactionSettings`, context-window percentage math);
//! - keep-recent-tail cut points and split turns (`findCutPoint`,
//!   `prepareCompaction`);
//! - branch summarization (`compaction/branch-summarization.ts`);
//! - file-operation tracking appended to summaries (`utils.ts`
//!   `extractFileOpsFromMessage`).
//!
//! pi-ai's `Message` union has no `compactionSummary` role, so the summary
//! rides as a user message in the exact LLM form upstream's `convertToLlm`
//! produces: `COMPACTION_SUMMARY_PREFIX + summary + COMPACTION_SUMMARY_SUFFIX`
//! (`messages.ts`). The session log keeps the raw summary in the
//! `SessionEntry::Compaction` entry and [`compaction_summary_message`]
//! rebuilds that message on replay.

use futures::StreamExt;

use pi_agent::Agent;
use pi_ai::{
	AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Model, StopReason,
	StreamFn, StreamOptions, SystemContent,
};

/// Verbatim from `compaction/utils.ts` (`SUMMARIZATION_SYSTEM_PROMPT`).
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Verbatim from `compaction/compaction.ts` (`SUMMARIZATION_PROMPT`).
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Verbatim from `core/messages.ts` (`COMPACTION_SUMMARY_PREFIX`).
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Verbatim from `core/messages.ts` (`COMPACTION_SUMMARY_SUFFIX`).
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Maximum characters for a tool result in serialized summaries
/// (`TOOL_RESULT_MAX_CHARS` in `compaction/utils.ts`).
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Result of a successful compaction summarization call, before any history
/// or session-log mutation (`CompactionResult` upstream, reduced).
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
	pub summary: String,
	/// Estimated context tokens before compaction (`tokensBefore` upstream).
	pub tokens_before: u64,
}

/// The compaction product as it lives in the agent transcript: a user message
/// in upstream's `convertToLlm` wire form.
pub fn compaction_summary_message(summary: &str) -> Message {
	Message::user(format!(
		"{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
	))
}

/// Replacement history for a compacted transcript: system messages are kept
/// verbatim (upstream replays the effective system message into the
/// compaction entry), everything else collapses into the summary message.
pub fn replacement_history(messages: &[Message], summary: &str) -> Vec<Message> {
	let mut history: Vec<Message> = messages
		.iter()
		.filter(|message| matches!(message, Message::System(_)))
		.cloned()
		.collect();
	history.push(compaction_summary_message(summary));
	history
}

/// Summarize the agent's current transcript (everything except system
/// messages) with one standalone LLM call. Errors mirror upstream's
/// `getSummarizationFailure`: error/length stop reasons and tool calls in the
/// response refuse to become a checkpoint.
pub async fn compact_agent(agent: &Agent) -> anyhow::Result<CompactionOutcome> {
	let snapshot = agent.snapshot();
	let transcript: Vec<Message> = snapshot
		.messages
		.iter()
		.filter(|message| !matches!(message, Message::System(_)))
		.cloned()
		.collect();
	if transcript.is_empty() {
		anyhow::bail!("Nothing to compact (session too small)");
	}

	let tokens_before: u64 = snapshot.messages.iter().map(estimate_tokens).sum();
	let conversation = serialize_conversation(&transcript);
	// Wrap the serialized conversation in tags so the model treats it as data,
	// not as a conversation to continue (`buildSummarizationContext` upstream).
	let prompt_text =
		format!("<conversation>\n{conversation}\n</conversation>\n\n{SUMMARIZATION_PROMPT}");

	let model = agent.model();
	let stream_fn = agent.stream_fn();
	let response = complete_summarization(&model, &stream_fn, prompt_text).await?;
	if let Some(failure) = summarization_failure(&response) {
		anyhow::bail!("{failure}");
	}
	if !response.tool_calls().is_empty() {
		anyhow::bail!("Summarization attempted to call a tool");
	}
	Ok(CompactionOutcome {
		summary: response.text(),
		tokens_before,
	})
}

/// Run the single summarization request to completion
/// (`completeSummarization` upstream, without retry wiring). The request is
/// self-contained: the compaction system prompt plus one user message.
async fn complete_summarization(
	model: &Model,
	stream_fn: &StreamFn,
	prompt_text: String,
) -> anyhow::Result<AssistantMessage> {
	let context = pi_ai::normalize_context(
		Some(SUMMARIZATION_SYSTEM_PROMPT),
		&[],
		vec![Message::user(prompt_text)],
	);
	// One-off summaries get no session routing (upstream forces a fresh
	// routing id with cacheRetention "none"); the port's providers ignore it.
	let options = StreamOptions::default();
	let mut stream = stream_fn(model, &context, options);
	let mut final_message = None;
	while let Some(event) = stream.next().await {
		match event {
			AssistantMessageEvent::Done { message, .. } => final_message = Some(message),
			AssistantMessageEvent::Error { error, .. } => final_message = Some(error),
			_ => {}
		}
	}
	final_message
		.ok_or_else(|| anyhow::anyhow!("summarization stream ended without a terminal event"))
}

/// Port of `getSummarizationFailure`: a length stop holds partial text and
/// must not become a session checkpoint.
fn summarization_failure(response: &AssistantMessage) -> Option<String> {
	match response.stop_reason {
		StopReason::Error => Some(format!(
			"Summarization failed: {}",
			response.error_message.as_deref().unwrap_or("Unknown error")
		)),
		StopReason::Length => Some(
			"Summarization failed: generation hit the token cap and the summary is incomplete"
				.to_string(),
		),
		_ => None,
	}
}

/// Serialize non-system messages to text for summarization — port of
/// `serializeConversation` (`compaction/utils.ts`), reduced to the four
/// standard roles. Thinking is omitted and tool results are truncated so the
/// summarization request stays within reasonable budgets.
pub fn serialize_conversation(messages: &[Message]) -> String {
	let mut parts: Vec<String> = Vec::new();
	for message in messages {
		match message {
			Message::User(user) => {
				let content = user.content.as_text();
				if !content.is_empty() {
					parts.push(format!("[User]: {content}"));
				}
			}
			Message::Assistant(assistant) => {
				let tool_calls: Vec<String> = assistant
					.tool_calls()
					.iter()
					.map(|call| {
						let args = call
							.arguments
							.as_object()
							.map(|object| {
								object
									.iter()
									.map(|(key, value)| format!("{key}={value}"))
									.collect::<Vec<_>>()
									.join(", ")
							})
							.unwrap_or_default();
						format!("{}({args})", call.name)
					})
					.collect();
				let text = assistant.text();
				if !text.is_empty() {
					parts.push(format!("[Assistant]: {text}"));
				}
				if !tool_calls.is_empty() {
					parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
				}
			}
			Message::ToolResult(result) => {
				let content: String = result
					.content
					.iter()
					.filter_map(|block| block.text_content())
					.collect::<Vec<_>>()
					.join("\n");
				if !content.is_empty() {
					parts.push(format!(
						"[Tool result]: {}",
						truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
					));
				}
			}
			Message::System(_) => {}
		}
	}
	parts.join("\n\n")
}

/// Port of `truncateForSummary`: keep the head, append a truncation marker.
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
	let count = text.chars().count();
	if count <= max_chars {
		return text.to_string();
	}
	let head: String = text.chars().take(max_chars).collect();
	format!(
		"{head}\n\n[... {} more characters truncated]",
		count - max_chars
	)
}

/// chars/4 heuristic — port of `estimateTokens` (`compaction/compaction.ts`),
/// reduced to the four standard roles.
fn estimate_tokens(message: &Message) -> u64 {
	let chars: usize = match message {
		Message::System(system) => match &system.content {
			SystemContent::Text(text) => text.len(),
			SystemContent::Blocks(blocks) => blocks.iter().map(|block| block.text.len()).sum(),
		},
		Message::User(user) => user.content.as_text().len(),
		Message::Assistant(assistant) => assistant
			.content
			.iter()
			.map(|block| match block {
				AssistantContent::Text(text) => text.text.len(),
				AssistantContent::Thinking(thinking) => thinking.thinking.len(),
				AssistantContent::ToolCall(call) => {
					call.name.len() + call.arguments.to_string().len()
				}
			})
			.sum(),
		Message::ToolResult(result) => result
			.content
			.iter()
			.filter_map(|block| block.text_content())
			.map(str::len)
			.sum(),
	};
	chars.div_ceil(4) as u64
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_ai::{SystemMessage, ToolResultMessage, UserMessage};

	fn assistant_with_text(text: &str) -> Message {
		Message::Assistant(AssistantMessage {
			content: vec![AssistantContent::Text(pi_ai::TextContent {
				text: text.to_string(),
				text_signature: None,
			})],
			api: "mock".into(),
			provider: "mock".into(),
			model: "mock".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: Default::default(),
			stop_reason: StopReason::Stop,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		})
	}

	#[test]
	fn serialize_conversation_renders_roles_and_truncates_results() {
		let messages = vec![
			Message::System(SystemMessage::prompt("prompt")),
			Message::User(UserMessage::text("hello")),
			assistant_with_text("hi there"),
			Message::ToolResult(ToolResultMessage::text_result(
				"call-1", "ls", "a\nb", false,
			)),
		];
		let text = serialize_conversation(&messages);
		assert_eq!(
			text,
			"[User]: hello\n\n[Assistant]: hi there\n\n[Tool result]: a\nb"
		);
		// The system prompt is prompt state, not conversation content.
		assert!(!text.contains("prompt"));
	}

	#[test]
	fn long_tool_results_keep_head_and_marker() {
		let long = "x".repeat(TOOL_RESULT_MAX_CHARS + 500);
		let messages = vec![Message::ToolResult(ToolResultMessage::text_result(
			"call-1", "read", &long, false,
		))];
		let text = serialize_conversation(&messages);
		let expected_head = format!("[Tool result]: {}", "x".repeat(TOOL_RESULT_MAX_CHARS));
		assert!(text.starts_with(&expected_head));
		assert!(text.contains("[... 500 more characters truncated]"));
	}

	#[test]
	fn replacement_history_keeps_system_and_wraps_summary() {
		let messages = vec![
			Message::System(SystemMessage::prompt("prompt")),
			Message::User(UserMessage::text("old")),
			assistant_with_text("old reply"),
		];
		let history = replacement_history(&messages, "the summary");
		assert_eq!(history.len(), 2);
		assert!(matches!(history[0], Message::System(_)));
		let Message::User(summary) = &history[1] else {
			panic!("expected user-form summary message")
		};
		assert_eq!(
			summary.content.as_text(),
			format!("{COMPACTION_SUMMARY_PREFIX}the summary{COMPACTION_SUMMARY_SUFFIX}")
		);
	}

	#[test]
	fn estimate_tokens_uses_chars_over_four() {
		assert_eq!(
			estimate_tokens(&Message::User(UserMessage::text("abcd"))),
			1
		);
		assert_eq!(
			estimate_tokens(&Message::User(UserMessage::text("abcde"))),
			2
		);
	}
}
