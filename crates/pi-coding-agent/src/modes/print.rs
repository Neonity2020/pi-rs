//! Print mode — port of `pi/packages/coding-agent/src/modes/print-mode` paths
//! behind `pi -p`: run one prompt to completion, write the transcript to
//! stdout, exit non-zero on error turns.

use pi_agent::AgentEvent;
use pi_ai::Message;

use crate::core::agent_session::{SessionCommand, SessionEvent, SessionHandle};

pub async fn run_print(session: SessionHandle, prompt: String) -> anyhow::Result<i32> {
	let mut events = session.events;
	session
		.cmd_tx
		.send(SessionCommand::Prompt { text: prompt })?;

	let mut exit_code = 0;
	loop {
		let event = match events.recv().await {
			Ok(event) => event,
			Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
			Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
				eprintln!("warning: dropped {count} events");
				continue;
			}
		};
		match event {
			SessionEvent::Agent(event) => match *event {
				AgentEvent::MessageEnd {
					message: Message::Assistant(m),
				} => {
					if m.stop_reason == pi_ai::StopReason::Error {
						exit_code = 1;
					}
					let text = m.text();
					if !text.is_empty() {
						println!("{text}");
					}
					if let Some(error) = &m.error_message {
						eprintln!("error: {error}");
					}
				}
				AgentEvent::AgentEnd { .. } => break,
				_ => {}
			},
			SessionEvent::Notice(text) => eprintln!("· {text}"),
			SessionEvent::Persisted { .. } => {}
		}
	}

	let _ = session.cmd_tx.send(SessionCommand::Exit);
	Ok(exit_code)
}
