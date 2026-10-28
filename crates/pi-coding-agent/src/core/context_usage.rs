//! Context usage calculation for the footer and compaction threshold.
//! Mirrors the upstream usage preference and trailing-message estimation;
//! the per-usage total comes from `Usage::context_tokens`
//! (`calculateContextTokens` upstream).

use pi_agent::Agent;
use pi_ai::{Message, StopReason};

use super::compaction::estimate_tokens;

/// Returns `(context_tokens, context_window)`, or `None` when unknown.
pub fn compute(agent: &Agent) -> Option<(u64, u64)> {
	let window = agent.model().context_window;
	if window == 0 {
		return None;
	}
	let snapshot = agent.snapshot();
	estimate(&snapshot.messages, snapshot.usage_valid_from).map(|tokens| (tokens, window))
}

fn estimate(messages: &[Message], start: usize) -> Option<u64> {
	let start = start.min(messages.len());
	let latest = messages
		.iter()
		.enumerate()
		.skip(start)
		.rev()
		.find_map(|(index, message)| match message {
			Message::Assistant(assistant)
				if !matches!(
					assistant.stop_reason,
					StopReason::Aborted | StopReason::Error
				) =>
			{
				let count = assistant.usage.context_tokens();
				(count > 0).then_some((index, count))
			}
			_ => None,
		});
	if let Some((index, count)) = latest {
		Some(messages[index + 1..].iter().fold(count, |total, message| {
			total.saturating_add(estimate_tokens(message))
		}))
	} else if start > 0 {
		None
	} else {
		Some(messages.iter().map(estimate_tokens).sum())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_ai::{AssistantMessage, Model};

	#[test]
	fn totals_prefer_reported_total_and_include_output_in_fallback() {
		let mut usage = pi_ai::Usage {
			input: 10,
			output: 2,
			cache_read: 3,
			cache_write: 4,
			..Default::default()
		};
		assert_eq!(usage.context_tokens(), 19);
		usage.total_tokens = 25;
		assert_eq!(usage.context_tokens(), 25);
	}

	#[test]
	fn estimates_trailing_messages_and_invalidates_after_compaction() {
		let model = Model::new(
			"test",
			"test",
			"openai-completions",
			"test",
			"http://localhost",
		);
		let mut assistant = AssistantMessage::error(&model, StopReason::Error, "error");
		assistant.stop_reason = StopReason::Stop;
		assistant.usage.total_tokens = 100;
		let messages = vec![
			Message::Assistant(assistant.clone()),
			Message::user("12345678"),
		];
		assert_eq!(estimate(&messages, 0), Some(102));
		let compacted = vec![
			Message::Assistant(assistant.clone()),
			super::super::compaction::compaction_summary_message("summary"),
		];
		assert_eq!(estimate(&compacted, 2), None);
		let mut after = compacted;
		after.push(Message::Assistant(assistant.clone()));
		assert_eq!(estimate(&after, 2), Some(100));

		// A newer response with all-zero usage is skipped for an older valid
		// one; a zero-usage tail keeps the older usage plus the estimate.
		let mut zero = assistant.clone();
		zero.usage = pi_ai::Usage::default();
		let mixed = vec![
			Message::Assistant(assistant),
			Message::Assistant(zero),
			Message::user("123456"),
		];
		assert_eq!(estimate(&mixed, 0), Some(102));
	}
}
