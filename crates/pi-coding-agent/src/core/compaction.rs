//! Context compaction — port of
//! `pi/packages/coding-agent/src/core/compaction/compaction.ts` and
//! `.../compaction/utils.ts` (summarization prompts, conversation
//! serialization, failure checks), plus the compaction-summary message
//! conversion from `pi/packages/coding-agent/src/core/messages.ts`.
//!
//! A compaction summarizes the transcript prefix up to a cut point with one
//! standalone LLM call through the agent's own model/stream_fn and replaces
//! the history with `[system messages…, compaction summary, kept tail…]`.
//! Without an explicit cut (manual `/compact` on a small session) the whole
//! non-system transcript is summarized — upstream's "keep nothing" cut
//! (`firstKeptEntryId = entry id`). When the transcript already carries a
//! compaction summary, its text rides along as `<previous-summary>` context
//! and [`UPDATE_SUMMARIZATION_PROMPT`] merges it with the new range.
//!
//! Not ported yet (listed against the upstream source):
//! - per-model compaction settings (`CompactionSettings.modelOverrides`);
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

/// Verbatim from `compaction/compaction.ts` (`UPDATE_SUMMARIZATION_PROMPT`,
/// which upstream templates from `UPDATE_SUMMARIZATION_INSTRUCTIONS`; kept as
/// one literal because `concat!` accepts no consts).
pub const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Verbatim from `compaction/compaction.ts`
/// (`TURN_PREFIX_SUMMARIZATION_PROMPT`): the second summary of a split turn,
/// covering the head of an oversized single turn.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\nCreate a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.\n\n## Original Request\n[What did the user ask for?]\n\n## Progress So Far\n- [Key decisions and work completed in these messages]\n\n## Context Needed to Continue\n- [Information from these messages needed to understand the later work]\n\nOnly summarize information explicitly present above. Do not infer or recreate later messages.";

/// Markdown boundary merging the two summaries of a split turn
/// (`compaction.ts` `compact()` merge template).
pub const SPLIT_TURN_HISTORY_SEPARATOR: &str = "\n\n---\n\n";
pub const SPLIT_TURN_LABEL: &str = "**Turn Context (split turn):**\n\n";
/// Stand-in history text when a split turn has nothing before it
/// (`compaction.ts`: `previousSummary ?? "No prior history."`).
pub const NO_PRIOR_HISTORY_TEXT: &str = "No prior history.";

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
	pub kept: Vec<Message>,
	pub first_kept_id: Option<String>,
	pub source_messages: Vec<Message>,
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

/// Whether a compaction cut may land on this message (`isCutPointMessage`
/// upstream): user or assistant boundaries only. A cut between an assistant
/// and its tool results would strand the results in the summarized range
/// while their tool call is kept (or vice versa), so tool results — and
/// system prompt state — are never cut points. Cutting at an assistant with
/// tool calls is safe: its results follow it and are kept with the tail.
pub fn is_cut_point_message(message: &Message) -> bool {
	matches!(message, Message::User(_) | Message::Assistant(_))
}

/// Whether a message starts a turn (`isTurnStartMessage` upstream, reduced to
/// the roles this port's flat transcript holds): only user messages do.
/// The compaction-summary message rides as a user message, matching
/// upstream's `compactionSummary` turn-start role.
fn is_turn_start_message(message: &Message) -> bool {
	matches!(message, Message::User(_))
}

/// Where a compaction cut lands (`CutPointResult` upstream, over the flat
/// non-system transcript this port compacts): the first kept index, plus —
/// when the cut splits an oversized single turn — the index of the user
/// message that started that turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPoint {
	pub first_kept: usize,
	/// `Some(index)` when `first_kept` is not a turn start: the leading user
	/// message of the turn being split (upstream `turnStartIndex`).
	pub turn_start: Option<usize>,
}

/// First kept index of the keep-recent tail (`findProjectedCutPoint`
/// upstream, over the flat non-system message array this port compacts).
/// Walks backwards accumulating `estimate_tokens` until `keep_recent_tokens`
/// is reached and cuts at the closest valid cut point at or after that
/// message; when trailing tool results exceed the budget by themselves, their
/// preceding assistant tool call is kept instead of falling back to the first
/// message. A cut landing inside a multi-assistant turn reports the turn's
/// leading user message as `turn_start` (split turn — upstream
/// `findTurnStartIndex`). Returns first_kept 0 when nothing can be
/// summarized (no valid cut point, or the budget is never reached before the
/// first message).
///
/// Upstream's two entry-level adjustments do not apply on the flat message
/// array: there are no metadata-only entries to skip backwards over, and the
/// recovery-omission suffix advancement is structurally unnecessary — the
/// port omits the failed attempt from the live transcript before computing
/// the cut, so no phantom entries exist.
pub fn find_cut_point_detailed(messages: &[Message], keep_recent_tokens: u64) -> CutPoint {
	let cut_points: Vec<usize> = (0..messages.len())
		.filter(|&index| is_cut_point_message(&messages[index]))
		.collect();
	if cut_points.is_empty() {
		return CutPoint {
			first_kept: 0,
			turn_start: None,
		};
	}
	let mut accumulated = 0u64;
	let mut cut = cut_points[0];
	for index in (0..messages.len()).rev() {
		let tokens = estimate_tokens(&messages[index]);
		if tokens == 0 {
			continue;
		}
		accumulated = accumulated.saturating_add(tokens);
		if accumulated >= keep_recent_tokens {
			cut = cut_points
				.iter()
				.find(|&&candidate| candidate >= index)
				.copied()
				.unwrap_or(*cut_points.last().unwrap_or(&0));
			break;
		}
	}
	// Split-turn detection: a cut on an assistant message inside a turn
	// reports the turn's leading user message.
	let turn_start = if is_turn_start_message(&messages[cut]) {
		None
	} else {
		(0..=cut)
			.rev()
			.find(|&index| is_turn_start_message(&messages[index]))
	};
	CutPoint {
		first_kept: cut,
		turn_start,
	}
}

/// First kept index only — the pre-split-turn signature, kept for callers
/// that do not split turns.
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> usize {
	find_cut_point_detailed(messages, keep_recent_tokens).first_kept
}

/// Raw summary text carried by the transcript's last compaction-summary
/// message, if any (upstream reads it off the projected compaction entry).
fn previous_summary(messages: &[Message]) -> Option<(usize, String)> {
	let index = messages.iter().rposition(|message| {
		matches!(message, Message::User(user) if user.content.as_text().starts_with(COMPACTION_SUMMARY_PREFIX))
	})?;
	let Message::User(user) = &messages[index] else {
		return None;
	};
	let text = user.content.as_text();
	let inner = text
		.strip_prefix(COMPACTION_SUMMARY_PREFIX)
		.unwrap_or(&text);
	let inner = inner
		.strip_suffix(COMPACTION_SUMMARY_SUFFIX)
		.unwrap_or(inner);
	Some((index, inner.to_string()))
}

/// Summarize the agent's current transcript (everything except system
/// messages) with one standalone LLM call. Errors mirror upstream's
/// `getSummarizationFailure`: error/length stop reasons and tool calls in the
/// response refuse to become a checkpoint.
/// Explicit compaction boundary for [`compact_prefix`]: the first kept
/// transcript index (its durable log entry id when known), plus the turn
/// start when the cut splits an oversized single turn.
#[derive(Debug, Clone, Default)]
pub struct Cut {
	pub first_kept: usize,
	/// Leading user message of the split turn (`None` when the cut lands on
	/// a turn boundary).
	pub turn_start: Option<usize>,
	pub first_kept_id: Option<String>,
}

pub async fn compact_agent(agent: &Agent) -> anyhow::Result<CompactionOutcome> {
	compact_prefix(agent, None).await
}

pub async fn compact_prefix(agent: &Agent, cut: Option<Cut>) -> anyhow::Result<CompactionOutcome> {
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
	let (prefix, kept, first_kept_id) = if let Some(cut) = cut {
		if cut.first_kept == 0 || cut.first_kept >= transcript.len() {
			anyhow::bail!("invalid compaction boundary");
		}
		let turn_start = cut.turn_start.filter(|&start| start < cut.first_kept);
		(
			&transcript[..cut.first_kept],
			transcript[cut.first_kept..].to_vec(),
			(cut.first_kept_id, turn_start),
		)
	} else {
		(&transcript[..], Vec::new(), (None, None))
	};
	let (first_kept_id, turn_start) = first_kept_id;
	// A prior summary inside the summarized range is merged, not re-summarized:
	// it rides as <previous-summary> context (upstream `generateSummaryWithUsage`
	// passes `previousSummary` and switches to the update prompt), so the new
	// summary never nests the old one's full text twice.
	let (previous, previous_index) = match previous_summary(prefix) {
		Some((index, summary)) => (Some(summary), Some(index)),
		None => (None, None),
	};

	let model = agent.model();
	let stream_fn = agent.stream_fn();
	let summary = if let Some(turn_start) = turn_start {
		// Split turn (upstream `compact()`): the messages before the turn and
		// the turn's own prefix are summarized separately, then merged behind
		// a clear Markdown boundary.
		let (history, turn_prefix) = (&prefix[..turn_start], &prefix[turn_start..]);
		let history_text = if history.is_empty() && previous.is_none() {
			None
		} else {
			let history_to_summarize: Vec<Message> = history
				.iter()
				.enumerate()
				.filter(|&(index, _)| Some(index) != previous_index)
				.map(|(_, message)| message.clone())
				.collect();
			Some(if history_to_summarize.is_empty() {
				// Only the (already merged) previous summary remains.
				previous.clone().expect("guarded above")
			} else {
				complete_history_summary(&model, &stream_fn, history_to_summarize, previous.clone())
					.await?
			})
		};
		let history_text = history_text.unwrap_or_else(|| NO_PRIOR_HISTORY_TEXT.to_string());
		let turn_prefix_text =
			complete_turn_prefix_summary(&model, &stream_fn, turn_prefix).await?;
		format!("{history_text}{SPLIT_TURN_HISTORY_SEPARATOR}{SPLIT_TURN_LABEL}{turn_prefix_text}")
	} else {
		let to_summarize: Vec<Message> = prefix
			.iter()
			.enumerate()
			.filter(|&(index, _)| Some(index) != previous_index)
			.map(|(_, message)| message.clone())
			.collect();
		if to_summarize.is_empty() && kept.is_empty() {
			// Only an (already merged) summary is left — upstream prepareCompaction
			// returns undefined right after a compaction and `compact()` reports
			// "Already compacted".
			anyhow::bail!("Nothing to compact (session too small)");
		}
		let conversation = serialize_conversation(&to_summarize);
		// Wrap the serialized conversation in tags so the model treats it as data,
		// not as a conversation to continue (`buildSummarizationContext` upstream);
		// the base prompt follows upstream's ordering: conversation, previous
		// summary, then the instructions.
		let mut prompt_text = format!("<conversation>\n{conversation}\n</conversation>\n\n");
		if let Some(summary) = &previous {
			prompt_text.push_str(&format!(
				"<previous-summary>\n{summary}\n</previous-summary>\n\n"
			));
		}
		prompt_text.push_str(match &previous {
			Some(_) => UPDATE_SUMMARIZATION_PROMPT,
			None => SUMMARIZATION_PROMPT,
		});
		let response = complete_summarization(&model, &stream_fn, prompt_text).await?;
		check_summary_response(&response, "Summarization")?;
		response.text()
	};

	Ok(CompactionOutcome {
		summary,
		tokens_before,
		kept,
		first_kept_id,
		source_messages: snapshot.messages,
	})
}

/// History-range summary of a split turn (the ordinary summarization request,
/// with previous-summary merging).
async fn complete_history_summary(
	model: &Model,
	stream_fn: &StreamFn,
	to_summarize: Vec<Message>,
	previous: Option<String>,
) -> anyhow::Result<String> {
	let conversation = serialize_conversation(&to_summarize);
	let mut prompt_text = format!("<conversation>\n{conversation}\n</conversation>\n\n");
	if let Some(summary) = &previous {
		prompt_text.push_str(&format!(
			"<previous-summary>\n{summary}\n</previous-summary>\n\n"
		));
	}
	prompt_text.push_str(match &previous {
		Some(_) => UPDATE_SUMMARIZATION_PROMPT,
		None => SUMMARIZATION_PROMPT,
	});
	let response = complete_summarization(model, stream_fn, prompt_text).await?;
	check_summary_response(&response, "Summarization")?;
	Ok(response.text())
}

/// Turn-prefix summary of a split turn — upstream `generateTurnPrefixSummary`:
/// `# Conversation` framing instead of `<conversation>` tags, and the
/// dedicated turn-prefix instructions.
async fn complete_turn_prefix_summary(
	model: &Model,
	stream_fn: &StreamFn,
	turn_prefix: &[Message],
) -> anyhow::Result<String> {
	let conversation = serialize_conversation(turn_prefix);
	let prompt_text = format!(
		"# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
	);
	let response = complete_summarization(model, stream_fn, prompt_text).await?;
	check_summary_response(&response, "Turn prefix summarization")?;
	Ok(response.text())
}

/// Shared post-response checks (`getSummarizationFailure` + the tool-call
/// guard): error/length stops and tool calls must not become checkpoints.
fn check_summary_response(response: &AssistantMessage, label: &str) -> anyhow::Result<()> {
	if let Some(failure) = summarization_failure(response, label) {
		anyhow::bail!("{failure}");
	}
	if !response.tool_calls().is_empty() {
		anyhow::bail!("{label} attempted to call a tool");
	}
	Ok(())
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
fn summarization_failure(response: &AssistantMessage, label: &str) -> Option<String> {
	match response.stop_reason {
		StopReason::Error => Some(format!(
			"{label} failed: {}",
			response.error_message.as_deref().unwrap_or("Unknown error")
		)),
		StopReason::Length => Some(format!(
			"{label} failed: generation hit the token cap and the summary is incomplete"
		)),
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
pub(crate) fn estimate_tokens(message: &Message) -> u64 {
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

	fn tool_result(chars: usize) -> Message {
		Message::ToolResult(ToolResultMessage::text_result(
			"call-1",
			"ls",
			"x".repeat(chars),
			false,
		))
	}

	/// ~400 tokens of text per message ("x" * 1600).
	fn heavy_user() -> Message {
		Message::user("x".repeat(1600))
	}

	#[test]
	fn cut_point_walks_backwards_to_the_budget() {
		let messages = vec![heavy_user(), heavy_user(), heavy_user(), heavy_user()];
		// Budget 500: the newest message alone (~400) stays under, two (~800)
		// exceed it → the tail from the second-newest message is kept.
		assert_eq!(find_cut_point(&messages, 500), 2);
		// Budget far above the transcript: never reached → nothing summarized.
		assert_eq!(find_cut_point(&messages, 100_000), 0);
	}

	#[test]
	fn cut_point_never_lands_on_a_tool_result() {
		// The budget is exceeded while walking the trailing tool result; with
		// no valid cut point at or after it, the cut falls back to the closest
		// preceding assistant so its tool result stays with the kept tail.
		let messages = vec![
			Message::user("x".repeat(400)),
			assistant_with_text("tool turn"),
			tool_result(1600),
		];
		assert_eq!(find_cut_point(&messages, 400), 1);
		// Exceeded only at the first message → nothing summarizable.
		assert_eq!(find_cut_point(&messages, 503), 0);
		// A later user message is preferred over stranding the tool result.
		let messages = vec![
			heavy_user(),
			assistant_with_text("tool turn"),
			tool_result(1600),
			heavy_user(),
		];
		assert_eq!(find_cut_point(&messages, 500), 3);
	}

	#[test]
	fn cut_point_without_valid_boundaries_keeps_everything() {
		let messages = vec![tool_result(1600), tool_result(1600)];
		assert_eq!(find_cut_point(&messages, 1), 0);
	}

	#[test]
	fn cut_point_inside_a_turn_reports_the_turn_start() {
		// Budget exceeded while walking the trailing tool result: the cut
		// falls back to the assistant (a valid cut point that is not a turn
		// start), so the turn's leading user message is reported for
		// split-turn handling.
		let big_assistant = "x".repeat(1600);
		let messages = vec![
			heavy_user(),
			assistant_with_text(&big_assistant),
			tool_result(1600),
		];
		let cut = find_cut_point_detailed(&messages, 600);
		assert_eq!(cut.first_kept, 1);
		assert_eq!(cut.turn_start, Some(0));
		// A cut that lands on a user message never reports a split.
		let messages = vec![
			heavy_user(),
			assistant_with_text(&big_assistant),
			tool_result(1600),
			heavy_user(),
		];
		let cut = find_cut_point_detailed(&messages, 500);
		assert_eq!(cut.first_kept, 3);
		assert_eq!(cut.turn_start, None);
	}

	/// Capturing summarizer: history-range requests echo "[history summary]",
	/// turn-prefix requests echo "[turn-prefix summary]".
	fn split_turn_stream_fn(prompts: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> StreamFn {
		std::sync::Arc::new(move |_model, context, _| {
			let prompt = match context.messages.last() {
				Some(Message::User(user)) => user.content.as_text(),
				_ => String::new(),
			};
			let reply = if prompt.starts_with("# Conversation\n") {
				"[turn-prefix summary]"
			} else {
				"[history summary]"
			};
			prompts.lock().unwrap().push(prompt);
			let Message::Assistant(message) = assistant_with_text(reply) else {
				unreachable!("assistant_with_text builds an assistant message")
			};
			Box::pin(futures::stream::once(async move {
				pi_ai::AssistantMessageEvent::Done {
					reason: StopReason::Stop,
					message,
				}
			})) as pi_ai::AssistantMessageEventStream
		})
	}

	#[tokio::test]
	async fn split_turn_compaction_merges_two_summaries() {
		use std::sync::Arc;
		let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
		let agent = Agent::new(pi_agent::AgentOptions {
			stream_fn: Some(split_turn_stream_fn(prompts.clone())),
			..Default::default()
		});
		agent
			.replace_messages(vec![
				Message::System(pi_ai::SystemMessage::prompt("prompt")),
				Message::user("old work"),
				assistant_with_text("old reply"),
				// One oversized turn: user request, two assistant steps.
				Message::user("big single-turn request"),
				assistant_with_text("step one"),
				tool_result(1600),
				assistant_with_text("step two"),
				tool_result(1600),
			])
			.unwrap();
		// The cut lands inside the turn: keeping starts at "step two", the
		// turn's head (from its leading user message) is summarized too.
		let outcome = compact_prefix(
			&agent,
			Some(Cut {
				first_kept: 5,
				turn_start: Some(2),
				first_kept_id: Some("kept-entry".into()),
			}),
		)
		.await
		.unwrap();
		let captured = prompts.lock().unwrap();
		assert_eq!(captured.len(), 2, "one summary per range");

		let history = &captured[0];
		assert!(history.contains("[User]: old work"));
		assert!(!history.contains("big single-turn request"));
		assert!(history.ends_with(SUMMARIZATION_PROMPT));

		let turn_prefix = &captured[1];
		assert!(
			turn_prefix.starts_with("# Conversation\n"),
			"turn-prefix requests use the # Conversation framing: {turn_prefix}"
		);
		assert!(turn_prefix.contains("[User]: big single-turn request"));
		assert!(turn_prefix.contains("[Assistant]: step one"));
		assert!(turn_prefix.ends_with(TURN_PREFIX_SUMMARIZATION_PROMPT));

		// Upstream merge template: `{history}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}`.
		assert_eq!(
			outcome.summary,
			"[history summary]\n\n---\n\n**Turn Context (split turn):**\n\n[turn-prefix summary]"
		);
		// The kept tail and the durable boundary ride through untouched.
		assert_eq!(outcome.kept.len(), 2); // "step two" + its tool result
		assert_eq!(outcome.first_kept_id.as_deref(), Some("kept-entry"));
	}

	#[tokio::test]
	async fn split_turn_with_no_prior_history_says_so() {
		use std::sync::Arc;
		let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
		let agent = Agent::new(pi_agent::AgentOptions {
			stream_fn: Some(split_turn_stream_fn(prompts.clone())),
			..Default::default()
		});
		agent
			.replace_messages(vec![
				Message::System(pi_ai::SystemMessage::prompt("prompt")),
				Message::user("big single-turn request"),
				assistant_with_text("turn progress"),
				tool_result(100),
			])
			.unwrap();
		let outcome = compact_prefix(
			&agent,
			Some(Cut {
				first_kept: 2,
				turn_start: Some(0),
				first_kept_id: None,
			}),
		)
		.await
		.unwrap();
		assert_eq!(
			prompts.lock().unwrap().len(),
			1,
			"no history range to summarize"
		);
		assert!(
			outcome
				.summary
				.starts_with("No prior history.\n\n---\n\n**Turn Context (split turn):**\n\n"),
			"{}",
			outcome.summary
		);
		assert!(outcome.summary.contains("[turn-prefix summary]"));
	}

	#[tokio::test]
	async fn second_compaction_merges_the_previous_summary() {
		use std::sync::Arc;
		let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
		let capture = seen.clone();
		let stream: StreamFn = Arc::new(move |_model, context, _| {
			// The summarization request rides as one user message; capture it
			// and echo a fixed summary back so the assertions can inspect the
			// prompt shape through the recorded prompts.
			let prompt = match context.messages.last() {
				Some(Message::User(user)) => user.content.as_text(),
				_ => String::new(),
			};
			capture.lock().unwrap().push(prompt);
			let Message::Assistant(message) = assistant_with_text("[mock summary]") else {
				unreachable!("assistant_with_text builds an assistant message")
			};
			Box::pin(futures::stream::once(async move {
				pi_ai::AssistantMessageEvent::Done {
					reason: StopReason::Stop,
					message,
				}
			})) as pi_ai::AssistantMessageEventStream
		});
		let agent = Agent::new(pi_agent::AgentOptions {
			stream_fn: Some(stream),
			..Default::default()
		});
		agent
			.replace_messages(vec![
				Message::System(pi_ai::SystemMessage::prompt("prompt")),
				Message::user("older work"),
				assistant_with_text("older reply"),
			])
			.unwrap();
		let first = compact_agent(&agent).await.unwrap();
		assert!(first.kept.is_empty());
		assert!(seen.lock().unwrap()[0].ends_with(SUMMARIZATION_PROMPT));
		// Apply the first compaction, add a fresh turn, then compact again.
		let mut merged = replacement_history(&first.source_messages, &first.summary);
		merged.push(Message::user("newer work"));
		agent.replace_messages(merged).unwrap();
		let second = compact_agent(&agent).await.unwrap();
		let prompts = seen.lock().unwrap();
		let second_prompt = &prompts[1];
		// The old summary text is passed as <previous-summary> context and the
		// update instructions replace the initial prompt verbatim.
		assert!(second_prompt.contains("<previous-summary>"));
		assert!(second_prompt.contains("[mock summary]"));
		assert!(second_prompt.contains("[User]: newer work"));
		assert!(second_prompt.ends_with(UPDATE_SUMMARIZATION_PROMPT));
		assert_eq!(second.summary, "[mock summary]");
	}
}
