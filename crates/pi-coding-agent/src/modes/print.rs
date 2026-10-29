//! Print mode — port of `pi/packages/coding-agent/src/modes/print-mode` paths
//! behind `pi -p` and `pi --mode json`: run the supplied prompts to completion
//! in order, then either write the last assistant text to stdout (`text`) or
//! stream every session event as JSONL (`json`).
//!
//! Exit codes follow upstream print-mode.ts: `error`/`aborted`/`length` turn
//! exit 1 — but only in text mode; the json branch never sets an exit code
//! (failures are visible as `message_end` events with an error stop reason),
//! so json mode always exits 0 unless the run itself panics.
//!
//! Deviations (module-level, both modes): no stdout backpressure handling and
//! no SIGTERM/SIGHUP traps (upstream waits on raw-stdout backpressure and
//! exits 143/129). JSON-mode specifics live in `core/json_event.rs`: notices
//! go to stderr (upstream has no session-event equivalent) and `agent_settled`
//! is the bare upstream frame without `aborted`.

use std::io::Write;

use pi_agent::AgentEvent;
use pi_ai::{AssistantContent, Message, StopReason};

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle};
use crate::core::json_event::to_json_event;

/// Output mode — `--mode json` streams events, the default `text` mode
/// reports the final assistant message (upstream `PrintModeOptions.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintOutputMode {
	Text,
	Json,
}

pub async fn run_print(
	session: SessionHandle,
	mode: PrintOutputMode,
	header_line: Option<String>,
	prompts: Vec<String>,
) -> anyhow::Result<i32> {
	let stdout = std::io::stdout();
	let mut out = stdout.lock();
	run_print_to(&mut out, session, mode, header_line, prompts).await
}

/// Testable core of print mode: same behavior as [`run_print`] with the
/// output sink injected (`text` lines and JSON frames both go through it).
async fn run_print_to<W: Write>(
	out: &mut W,
	session: SessionHandle,
	mode: PrintOutputMode,
	header_line: Option<String>,
	prompts: Vec<String>,
) -> anyhow::Result<i32> {
	let mut events = session.events;

	// print-mode.ts:122-127: the JSON stream opens with the session header
	// line when the session has one. pi-rs in-memory sessions (`--no-session`)
	// carry no header, so the line is omitted there — upstream in-memory
	// sessions still have a header object (declared deviation).
	if mode == PrintOutputMode::Json
		&& let Some(header_line) = header_line
	{
		writeln!(out, "{header_line}")?;
		out.flush()?;
	}

	let mut last_message: Option<Message> = None;
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
					if let AgentEvent::MessageEnd { message } = event.as_ref() {
						last_message = Some(message.clone());
					}
					let is_agent_end = matches!(&*event, AgentEvent::AgentEnd { .. });
					if mode == PrintOutputMode::Json
						&& let Some(frame) = to_json_event(&SessionEvent::Agent(event))
					{
						// The upstream `agent_end` carries `willRetry`; the
						// session task ships one decision per forwarded end,
						// in order (same mechanism as the RPC wire patch).
						let frame = if is_agent_end {
							let will_retry = session
								.shared
								.agent_end_will_retry
								.lock()
								.unwrap()
								.pop_front()
								.unwrap_or(false);
							match serde_json::from_str::<serde_json::Value>(&frame) {
								Ok(mut value) => {
									value["willRetry"] = serde_json::json!(will_retry);
									value.to_string()
								}
								Err(_) => frame,
							}
						} else {
							frame
						};
						writeln!(out, "{frame}")?;
						out.flush()?;
					}
				}
				SessionEvent::Settled { aborted } => {
					if mode == PrintOutputMode::Json
						&& let Some(frame) = to_json_event(&SessionEvent::Settled { aborted })
					{
						writeln!(out, "{frame}")?;
						out.flush()?;
					}
					settled = true;
				}
				// Notices have no JSON wire counterpart upstream; they stay on
				// stderr so the stdout stream remains machine-readable.
				SessionEvent::Notice(text) => match mode {
					PrintOutputMode::Text => eprintln!("· {text}"),
					PrintOutputMode::Json => eprintln!("{text}"),
				},
				SessionEvent::Persisted { .. } => {}
			}
		}
	}

	// print-mode.ts:139-159: only the text branch reports the final assistant
	// message; the json branch leaves the exit code untouched (0).
	let exit_code = match mode {
		PrintOutputMode::Text => report_last_message(last_message.as_ref(), out)?,
		PrintOutputMode::Json => 0,
	};

	let _ = session.cmd_tx.send(SessionCommand::Exit);
	while !matches!(
		events.recv().await,
		Err(tokio::sync::broadcast::error::RecvError::Closed)
	) {}
	Ok(exit_code)
}

/// Upstream print-mode.ts:139-159 — only when the last transcript message is
/// an assistant message is anything reported: text blocks to stdout, error/
/// aborted diagnostics to stderr with exit 1, and a truncation note for
/// length stops.
fn report_last_message<W: Write>(message: Option<&Message>, out: &mut W) -> anyhow::Result<i32> {
	let Some(Message::Assistant(m)) = message else {
		return Ok(0);
	};
	match m.stop_reason {
		StopReason::Error => {
			eprintln!("{}", m.error_message.as_deref().unwrap_or("Request error"));
			Ok(1)
		}
		StopReason::Aborted => {
			eprintln!(
				"{}",
				m.error_message.as_deref().unwrap_or("Request aborted")
			);
			Ok(1)
		}
		StopReason::Length => {
			write_text_blocks(m, out)?;
			eprintln!("Response was truncated: the model reached its output token limit.");
			Ok(1)
		}
		_ => {
			write_text_blocks(m, out)?;
			Ok(0)
		}
	}
}

fn write_text_blocks<W: Write>(
	message: &pi_ai::AssistantMessage,
	out: &mut W,
) -> anyhow::Result<()> {
	for block in &message.content {
		if let AssistantContent::Text(t) = block {
			writeln!(out, "{}", t.text)?;
			out.flush()?;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::{Arc, Mutex};

	use pi_agent::{Agent, AgentOptions};
	use pi_ai::Model;
	use serde_json::{Value, json};

	fn mock_handle() -> SessionHandle {
		let stream: pi_ai::StreamFn = Arc::new(
			|model: &pi_ai::Model,
			 context: &pi_ai::TranscriptContext,
			 options: pi_ai::StreamOptions| {
				pi_ai::providers::mock::stream(model, context, options)
			},
		);
		let agent = Agent::new(AgentOptions {
			model: Model::new("mock-echo", "Mock", "mock", "mock", ""),
			stream_fn: Some(stream),
			..AgentOptions::default()
		});
		crate::core::agent_session::spawn_agent_session(
			agent,
			crate::core::session_manager::SessionManager::in_memory(),
			Arc::new(Mutex::new(
				crate::core::model_registry::ModelRegistry::load(None),
			)),
		)
	}

	fn parse(line: &str) -> Value {
		serde_json::from_str(line).expect("each output line is JSON")
	}

	#[tokio::test]
	async fn json_mode_opens_with_header_and_streams_the_event_protocol() {
		let handle = mock_handle();
		let mut out: Vec<u8> = Vec::new();
		let code = run_print_to(
			&mut out,
			handle,
			PrintOutputMode::Json,
			Some(r#"{"type":"session","id":"s1"}"#.into()),
			vec!["hello".into()],
		)
		.await
		.expect("run completes");
		assert_eq!(code, 0, "json mode never sets a nonzero exit code");

		let text = String::from_utf8(out).expect("utf-8 output");
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(
			parse(lines[0]),
			json!({"type": "session", "id": "s1"}),
			"first line is the session header"
		);
		let kinds: Vec<String> = lines
			.iter()
			.map(|line| parse(line)["type"].as_str().unwrap_or_default().to_string())
			.collect();
		assert_eq!(
			kinds[1], "agent_start",
			"the run opens the stream before the prompt is appended"
		);
		assert_eq!(*kinds.last().unwrap(), "agent_settled");
		assert!(kinds.contains(&"message_update".to_string()));
		assert!(kinds.contains(&"agent_end".to_string()));

		// message_update frames: usage at the top level, no partial/snapshot.
		let update = lines
			.iter()
			.map(|line| parse(line))
			.find(|frame| frame["type"] == "message_update")
			.expect("stream contains message updates");
		assert!(update["usage"].is_object());
		assert!(update.get("message").is_none());
		assert!(update["assistantMessageEvent"].get("partial").is_none());

		// The final assistant text is NOT repeated in json mode (text-mode
		// only behavior upstream).
		let message_end = lines
			.iter()
			.rev()
			.map(|line| parse(line))
			.find(|frame| frame["type"] == "message_end")
			.expect("message_end present");
		assert!(
			message_end["message"]["content"][0]["text"]
				.as_str()
				.unwrap()
				.contains("hello")
		);
		assert!(
			!lines.iter().any(|line| line.starts_with("hello")),
			"no bare text lines outside JSON frames"
		);
	}

	#[tokio::test]
	async fn json_mode_omits_header_line_when_none() {
		let handle = mock_handle();
		let mut out: Vec<u8> = Vec::new();
		run_print_to(
			&mut out,
			handle,
			PrintOutputMode::Json,
			None,
			vec!["hi".into()],
		)
		.await
		.expect("run completes");
		let text = String::from_utf8(out).expect("utf-8 output");
		assert_eq!(
			parse(text.lines().next().unwrap())["type"],
			"agent_start",
			"in-memory sessions write no header line"
		);
	}

	#[tokio::test]
	async fn text_mode_writes_only_the_final_assistant_text() {
		let handle = mock_handle();
		let mut out: Vec<u8> = Vec::new();
		let code = run_print_to(
			&mut out,
			handle,
			PrintOutputMode::Text,
			None,
			vec!["hello".into()],
		)
		.await
		.expect("run completes");
		assert_eq!(code, 0);
		let text = String::from_utf8(out).expect("utf-8 output");
		assert!(text.contains("hello"));
		assert!(
			!text.contains("message_update"),
			"no JSON frames in text mode"
		);
	}

	/// report_last_message mirrors print-mode.ts:139-159 exit codes.
	fn assistant_with(
		stop_reason: StopReason,
		error_message: Option<String>,
	) -> pi_ai::AssistantMessage {
		pi_ai::AssistantMessage {
			content: vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
				text: "output".into(),
				text_signature: None,
			})],
			api: "mock".into(),
			provider: "mock".into(),
			model: "mock-echo".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: pi_ai::Usage::default(),
			stop_reason,
			error_message,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		}
	}

	fn report_of(stop_reason: StopReason, error_message: Option<String>) -> i32 {
		let mut out: Vec<u8> = Vec::new();
		report_last_message(
			Some(&Message::Assistant(assistant_with(
				stop_reason,
				error_message,
			))),
			&mut out,
		)
		.unwrap()
	}

	#[test]
	fn text_exit_codes_follow_upstream_stop_reasons() {
		assert_eq!(report_of(StopReason::Stop, None), 0);
		assert_eq!(report_of(StopReason::ToolUse, None), 0);
		assert_eq!(report_of(StopReason::Error, Some("boom".into())), 1);
		assert_eq!(report_of(StopReason::Aborted, None), 1);
		assert_eq!(report_of(StopReason::Length, None), 1);
		// A non-assistant last message (upstream role check) reports nothing.
		let mut out: Vec<u8> = Vec::new();
		assert_eq!(
			report_last_message(
				Some(&Message::User(pi_ai::UserMessage::text("hi"))),
				&mut out
			)
			.unwrap(),
			0
		);
		assert_eq!(report_last_message(None, &mut out).unwrap(), 0);
	}

	#[test]
	fn length_still_writes_text_before_the_truncation_note() {
		let mut out: Vec<u8> = Vec::new();
		let code = report_last_message(
			Some(&Message::Assistant(assistant_with(
				StopReason::Length,
				None,
			))),
			&mut out,
		)
		.unwrap();
		assert_eq!(code, 1);
		assert!(String::from_utf8(out).unwrap().contains("output"));
	}
}
