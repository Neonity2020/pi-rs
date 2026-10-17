//! Context compaction, modelled on Pi: when the request would exceed the
//! model's window, summarize the old part of the conversation and keep a
//! recent tail. Pure functions live here; the provider call lives in
//! `Agent::compact`.
//!
//! Invariants borrowed from the rest of pi-rs:
//! - a compaction summary is a normal user message, so `llm::Message` stays
//!   the only message type;
//! - cut points never split an assistant tool call from its tool results;
//! - retained assistant reasoning is not rewritten (DeepSeek requires it
//!   verbatim), only whole older turns are dropped.

use std::collections::BTreeSet;

use crate::{Error, Message, Role, ToolSpec};

/// Tokens kept free for the model's response and provider overhead.
pub const RESERVE_TOKENS: u64 = 16_384;
/// Approximate size of the tail kept verbatim after a compaction.
pub const KEEP_RECENT_TOKENS: u64 = 20_000;

/// Pi estimates images as a fixed number of characters instead of counting
/// base64 bytes (which are not proportional to image tokens).
const IMAGE_CHARS: usize = 4_800;
/// Tool results are truncated in the summarization request. Full content is
/// not needed to summarize what happened.
const TOOL_RESULT_MAX_CHARS: usize = 2_000;
/// Hard ceiling for the serialized conversation sent to the summarizer.
const SUMMARY_MAX_CHARS: usize = 400_000;

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags. Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// True when `message` is the synthetic user message that carries a previous
/// compaction summary. It is not part of the conversation being summarized;
/// `previous_summary` holds it separately.
pub fn is_summary_message(message: &Message) -> bool {
    message.role == Role::User
        && message
            .content
            .as_deref()
            .is_some_and(|text| text.starts_with(COMPACTION_SUMMARY_PREFIX))
}

pub fn summary_message(summary: &str) -> Message {
    Message::user(format!(
        "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
    ))
}

/// Conservative chars/4 estimate, matching Pi. Reasoning is included because
/// it is part of the request; images use a fixed budget.
pub fn estimate_tokens(message: &Message) -> u64 {
    let chars = match message.role {
        Role::Assistant => {
            let mut chars = message
                .content
                .as_deref()
                .map_or(0, |text| text.chars().count());
            chars += message
                .reasoning
                .as_deref()
                .map_or(0, |text| text.chars().count());
            for call in &message.tool_calls {
                chars += call.name.chars().count() + call.arguments.chars().count();
            }
            chars
        }
        Role::Tool => message
            .content
            .as_deref()
            .map_or(0, |text| text.chars().count()),
        Role::System | Role::User => {
            let mut chars = message
                .content
                .as_deref()
                .map_or(0, |text| text.chars().count());
            chars += message.images.len() * IMAGE_CHARS;
            chars
        }
    };
    chars.div_ceil(4) as u64
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_tokens).sum()
}

/// Full request estimate: the system prompt and tool schemas count too.
pub fn estimate_context_tokens(system: &str, tools: &[ToolSpec], messages: &[Message]) -> u64 {
    let mut chars = system.chars().count();
    for tool in tools {
        chars += tool.name.chars().count()
            + tool.description.chars().count()
            + tool.parameters.to_string().chars().count();
    }
    estimate_messages_tokens(messages) + chars.div_ceil(4) as u64
}

pub fn reserve_tokens(context_window: u64) -> u64 {
    if context_window == 0 {
        RESERVE_TOKENS
    } else {
        RESERVE_TOKENS.min(context_window / 4).max(1)
    }
}

pub fn keep_recent_tokens(context_window: u64) -> u64 {
    if context_window == 0 {
        KEEP_RECENT_TOKENS
    } else {
        KEEP_RECENT_TOKENS.min((context_window / 2).max(1)).max(1)
    }
}

/// Pi's threshold: compact once the request would eat into the reserve.
/// Unknown windows (`0`) never trigger automatically.
pub fn should_compact(tokens: u64, context_window: u64) -> bool {
    context_window > 0 && tokens > context_window.saturating_sub(reserve_tokens(context_window))
}

/// Find the first message to keep. Cut points are user or assistant messages,
/// never tool results. Walking backwards keeps approximately `keep` tokens.
/// `start` skips a leading summary message.
pub fn find_cut_index(messages: &[Message], start: usize, keep: u64) -> Option<usize> {
    let cut_points: Vec<usize> = (start..messages.len())
        .filter(|&i| matches!(messages[i].role, Role::User | Role::Assistant))
        .collect();
    if cut_points.is_empty() {
        return None;
    }

    let mut accumulated = 0u64;
    let mut cut = cut_points[0];
    let mut reached = false;
    for i in (start..messages.len()).rev() {
        let tokens = estimate_tokens(&messages[i]);
        if tokens == 0 {
            continue;
        }
        accumulated = accumulated.saturating_add(tokens);
        if accumulated >= keep {
            reached = true;
            cut = cut_points
                .iter()
                .copied()
                .find(|&candidate| candidate >= i)
                .unwrap_or_else(|| *cut_points.last().expect("non-empty"));
            break;
        }
    }

    if !reached || cut >= messages.len() {
        return None;
    }
    if cut <= start {
        // A single early message exceeded the budget. Summarize it and keep
        // the next valid cut point instead of declaring nothing to compact.
        cut = *cut_points.iter().find(|&&candidate| candidate > start)?;
    }
    (cut > start && cut < messages.len()).then_some(cut)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!(
        "{kept}\n\n[... {} more characters truncated]",
        text.chars().count() - max
    )
}

/// Serialize old messages as text so the summarizer does not try to continue
/// the conversation. Thinking is omitted; tool results are truncated.
pub fn serialize_conversation(messages: &[Message], max_chars: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut part = String::new();
                if let Some(text) = &message.content {
                    if !text.is_empty() {
                        part.push_str("[User]: ");
                        part.push_str(text);
                    }
                }
                for image in &message.images {
                    if !part.is_empty() {
                        part.push('\n');
                    }
                    part.push_str(&format!("[Image attached: {}]", image.media_type));
                }
                if !part.is_empty() {
                    parts.push(part);
                }
            }
            Role::Assistant => {
                if let Some(text) = &message.content {
                    if !text.is_empty() {
                        parts.push(format!("[Assistant]: {text}"));
                    }
                }
                if !message.tool_calls.is_empty() {
                    let calls = message
                        .tool_calls
                        .iter()
                        .map(|call| format!("{}({})", call.name, call.arguments))
                        .collect::<Vec<_>>()
                        .join("; ");
                    parts.push(format!("[Assistant tool calls]: {calls}"));
                }
            }
            Role::Tool => {
                if let Some(text) = &message.content {
                    if !text.is_empty() {
                        parts.push(format!(
                            "[Tool result]: {}",
                            truncate_chars(text, TOOL_RESULT_MAX_CHARS)
                        ));
                    }
                }
            }
            Role::System => {
                if let Some(text) = &message.content {
                    if !text.is_empty() {
                        parts.push(format!("[System]: {text}"));
                    }
                }
            }
        }
    }

    let mut out = String::new();
    for part in parts {
        if out.chars().count() >= max_chars {
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let remaining = max_chars.saturating_sub(out.chars().count());
        out.push_str(&truncate_chars(&part, remaining));
    }
    if out.chars().count() >= max_chars {
        out.push_str("\n\n[... conversation truncated for summarization]");
    }
    out
}

/// Budget for the serialized conversation. Kept well below the window so the
/// summarization request itself cannot overflow the model.
pub fn summary_char_budget(context_window: u64) -> usize {
    let budget = if context_window == 0 {
        // Unknown model: conservative but large enough for a meaningful summary.
        120_000
    } else {
        context_window.saturating_mul(2) as usize
    };
    budget.clamp(8_000, SUMMARY_MAX_CHARS)
}

pub fn summary_prompt(
    conversation: &str,
    previous_summary: Option<&str>,
    split_turn: bool,
) -> String {
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        ));
        prompt.push_str(UPDATE_SUMMARIZATION_PROMPT);
    } else {
        prompt.push_str(SUMMARIZATION_PROMPT);
    }
    if split_turn {
        prompt.push_str("\n\nNote: later messages are kept verbatim after this summary. Do not reconstruct them; summarize only the conversation above.");
    }
    prompt
}

#[derive(Default)]
pub struct FileOps {
    read: BTreeSet<String>,
    modified: BTreeSet<String>,
}

pub fn collect_file_ops(messages: &[Message]) -> FileOps {
    let mut ops = FileOps::default();
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for call in &message.tool_calls {
            let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
                continue;
            };
            let Some(path) = args.get("file_path").and_then(|value| value.as_str()) else {
                continue;
            };
            match call.name.as_str() {
                "read" => {
                    ops.read.insert(path.to_string());
                }
                "write" | "edit" => {
                    ops.modified.insert(path.to_string());
                }
                _ => {}
            }
        }
    }
    ops
}

/// File provenance appended to every summary, as in Pi.
pub fn format_file_ops(ops: &FileOps) -> String {
    let read_only: Vec<&String> = ops.read.difference(&ops.modified).collect();
    let modified: Vec<&String> = ops.modified.iter().collect();
    let mut sections = Vec::new();
    if !read_only.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_only
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !modified.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

/// Provider-specific wording for "the prompt is bigger than my window".
pub fn is_context_overflow(error: &Error) -> bool {
    let text = match error {
        Error::Api { body, .. } => body.to_ascii_lowercase(),
        Error::Stream(message) => message.to_ascii_lowercase(),
        _ => return false,
    };
    // Wording collected from Pi's overflow detector. Lowercased; substring
    // matching is enough because we only see the provider's error body.
    [
        "prompt is too long",
        "prompt too long",
        "prompt exceeds max length",
        "request_too_large",
        "input is too long",
        "exceeds the context window",
        "maximum context length",
        "exceeds the maximum",
        "input token count",
        "maximum prompt length",
        "reduce the length",
        "exceeds the limit of",
        "exceeds the available context",
        "greater than the context length",
        "context window exceeds",
        "exceeded model token limit",
        "too large for model",
        "model_context_window_exceeded",
        "range of input length",
        "context_length_exceeded",
        "context length",
        "too many tokens",
        "token limit exceeded",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCall;

    #[test]
    fn estimates_text_and_images() {
        assert_eq!(estimate_tokens(&Message::user("")), 0);
        assert_eq!(estimate_tokens(&Message::user("abcdefgh")), 2);
        let image = crate::Image::new("image/png", "AAAA");
        assert_eq!(
            estimate_tokens(&Message::user_with_images("", vec![image])),
            (IMAGE_CHARS as u64).div_ceil(4)
        );
    }

    #[test]
    fn estimates_assistant_reasoning_and_tool_calls() {
        let message = Message::assistant(
            Some("text".into()),
            Some("think".into()),
            vec![ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: r#"{"file_path":"a"}"#.into(),
            }],
        );
        let chars = 4 + 5 + 4 + 16;
        assert_eq!(estimate_tokens(&message), (chars as u64).div_ceil(4));
    }

    #[test]
    fn threshold_reserves_space_and_ignores_unknown_windows() {
        assert!(!should_compact(1_000, 0));
        assert!(!should_compact(100_000, 1_000_000));
        assert!(should_compact(984_000, 1_000_000));
        assert!(should_compact(7_000, 8_000));
    }

    #[test]
    fn cut_points_never_land_on_tool_results() {
        let messages = vec![
            Message::user("one"),
            Message::assistant(Some("a".into()), None, vec![]),
            Message::user("two"),
            Message::assistant(
                Some("b".into()),
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool("c1", "result"),
        ];
        // The cut walks back from the end; the tool result is not a candidate.
        let cut = find_cut_index(&messages, 0, 1).unwrap();
        assert!(cut > 0);
        assert_ne!(messages[cut].role, Role::Tool);
    }

    #[test]
    fn cut_keeps_a_recent_tail() {
        let messages = (0..10)
            .flat_map(|i| {
                [
                    Message::user(format!("u{i}")),
                    Message::assistant(Some(format!("a{i}")), None, vec![]),
                ]
            })
            .collect::<Vec<_>>();
        let cut = find_cut_index(&messages, 0, 5).unwrap();
        assert!(cut >= 15, "cut={cut}");
        assert!(messages.len() - cut <= 6);
    }

    #[test]
    fn small_sessions_have_nothing_to_compact() {
        let messages = vec![Message::user("hi")];
        assert_eq!(find_cut_index(&messages, 0, KEEP_RECENT_TOKENS), None);
    }

    #[test]
    fn summary_messages_are_recognizable() {
        let message = summary_message("## Goal\nship it");
        assert!(is_summary_message(&message));
        assert!(message.text().contains("<summary>"));
        assert!(!is_summary_message(&Message::user("hello")));
    }

    #[test]
    fn serialization_omits_reasoning_and_truncates_tool_results() {
        let long = "x".repeat(TOOL_RESULT_MAX_CHARS + 10);
        let messages = vec![
            Message::user("hello"),
            Message::assistant(Some("answer".into()), Some("SECRET".into()), vec![]),
            Message::tool("c1", long),
        ];
        let text = serialize_conversation(&messages, 100_000);
        assert!(text.contains("[User]: hello"));
        assert!(text.contains("[Assistant]: answer"));
        assert!(!text.contains("SECRET"));
        assert!(text.contains("more characters truncated"));
    }

    #[test]
    fn serialization_respects_the_budget() {
        let messages = vec![Message::user("x".repeat(100_000))];
        let text = serialize_conversation(&messages, 1_000);
        assert!(text.chars().count() < 1_100, "{}", text.chars().count());
        assert!(text.contains("truncated"));
    }

    #[test]
    fn summary_prompt_carries_the_previous_summary() {
        let initial = summary_prompt("conv", None, false);
        assert!(initial.contains("## Goal"));
        assert!(!initial.contains("<previous-summary>"));
        let update = summary_prompt("conv", Some("old"), false);
        assert!(update.contains("<previous-summary>\nold\n</previous-summary>"));
        assert!(update.contains("PRESERVE all existing information"));
        let split = summary_prompt("conv", None, true);
        assert!(split.contains("kept verbatim"));
    }

    #[test]
    fn file_ops_are_collected_and_deduplicated() {
        let messages = vec![
            Message::assistant(
                None,
                None,
                vec![
                    ToolCall {
                        id: "1".into(),
                        name: "read".into(),
                        arguments: r#"{"file_path":"a.rs"}"#.into(),
                    },
                    ToolCall {
                        id: "2".into(),
                        name: "edit".into(),
                        arguments: r#"{"file_path":"a.rs"}"#.into(),
                    },
                    ToolCall {
                        id: "3".into(),
                        name: "write".into(),
                        arguments: r#"{"file_path":"b.rs"}"#.into(),
                    },
                ],
            ),
            Message::assistant(
                None,
                None,
                vec![ToolCall {
                    id: "4".into(),
                    name: "read".into(),
                    arguments: r#"{"file_path":"b.rs"}"#.into(),
                }],
            ),
        ];
        let text = format_file_ops(&collect_file_ops(&messages));
        assert!(!text.contains("<read-files>"), "{text}");
        assert!(text.contains("<modified-files>\na.rs\nb.rs\n</modified-files>"));
    }

    #[test]
    fn overflow_detection_matches_provider_wording() {
        let api = |body: &str| Error::Api {
            status: 400,
            body: body.into(),
        };
        assert!(is_context_overflow(&api(
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#
        )));
        assert!(is_context_overflow(&api("context_length_exceeded")));
        assert!(is_context_overflow(&api("prompt is too long: 9000 tokens")));
        assert!(!is_context_overflow(&api("invalid api key")));
        assert!(!is_context_overflow(&Error::Cancelled));
    }
}
