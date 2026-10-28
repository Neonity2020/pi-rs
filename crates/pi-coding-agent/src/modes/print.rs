//! Print mode — port of `pi/packages/coding-agent/src/modes/print-mode` paths
//! behind `pi -p`: run the supplied prompts to completion in order, then write
//! the last assistant text to stdout. Exit codes follow upstream print-mode.ts:
//! error/aborted/length turns exit 1.

use pi_agent::AgentEvent;
use pi_ai::{AssistantContent, Message, StopReason};

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle};

pub async fn run_print(session: SessionHandle, prompts: Vec<String>) -> anyhow::Result<i32> {
	let mut events = session.events;

	let mut last_assistant: Option<pi_ai::AssistantMessage> = None;
	for prompt in prompts {
		session
			.cmd_tx
			.send(SessionCommand::Prompt { text: prompt })?;
		// The settle boundary is the quiescent point after a run (and any
		// auto-compaction it triggered), so the next prompt only starts
		// against a settled transcript — upstream awaits each
		// `session.prompt()` in turn.
		let mut settled = false;
		while !settled {
			let event = match events.recv().await {
				Ok(event) => event,
				// Session task shut down under us; treat as settled so the
				// remaining prompts drain (they will queue) and we exit.
				Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
				Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
					eprintln!("warning: dropped {count} events");
					continue;
				}
			};
			match event {
				SessionEvent::Agent(event) => {
					if let AgentEvent::MessageEnd {
						message: Message::Assistant(m),
					} = *event
					{
						last_assistant = Some(m);
					}
				}
				SessionEvent::Settled => settled = true,
				SessionEvent::Notice(text) => eprintln!("· {text}"),
				SessionEvent::Persisted { .. } => {}
			}
		}
	}

	let exit_code = report_last_assistant(last_assistant.as_ref());

	let _ = session.cmd_tx.send(SessionCommand::Exit);
	while !matches!(
		events.recv().await,
		Err(tokio::sync::broadcast::error::RecvError::Closed)
	) {}
	Ok(exit_code)
}

/// Upstream print-mode.ts:139-159 — only the last assistant message is
/// reported, after all prompts finish: text blocks to stdout, error/aborted
/// diagnostics to stderr with exit 1, and a truncation note for length stops.
fn report_last_assistant(assistant: Option<&pi_ai::AssistantMessage>) -> i32 {
	let Some(m) = assistant else {
		return 0;
	};
	match m.stop_reason {
		StopReason::Error => {
			eprintln!("{}", m.error_message.as_deref().unwrap_or("Request error"));
			1
		}
		StopReason::Aborted => {
			eprintln!(
				"{}",
				m.error_message.as_deref().unwrap_or("Request aborted")
			);
			1
		}
		StopReason::Length => {
			for block in &m.content {
				if let AssistantContent::Text(t) = block {
					println!("{}", t.text);
				}
			}
			eprintln!("Response was truncated: the model reached its output token limit.");
			1
		}
		_ => {
			for block in &m.content {
				if let AssistantContent::Text(t) = block {
					println!("{}", t.text);
				}
			}
			0
		}
	}
}
