//! RPC mode — port of `pi/packages/coding-agent/src/modes/rpc/rpc-mode.ts`.
//!
//! JSONL command/response/event framing over stdio. Framework stage: the
//! framing loop and the command dispatch table (`prompt`, `steer`,
//! `follow_up`, `abort`, `get_state`, `set_model`, ...) are stubbed; wire it
//! to `SessionCommand`/`SessionEvent` in the embeddable phase.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

pub async fn run_rpc() -> anyhow::Result<()> {
	let stdin = std::io::stdin();
	let mut stdout = std::io::stdout();
	let mut line = String::new();

	eprintln!("pi-rs rpc mode: framing accepted, command dispatch lands in the embeddable phase");
	while stdin.lock().read_line(&mut line)? > 0 {
		if line.trim().is_empty() {
			line.clear();
			continue;
		}
		let trimmed: String = line.trim().to_string();
		line.clear();
		let command: Value = match serde_json::from_str(&trimmed) {
			Ok(value) => value,
			Err(e) => {
				let response = json!({
					"type": "response",
					"command": null,
					"success": false,
					"error": format!("invalid JSON: {e}")
				});
				writeln!(stdout, "{response}")?;
				stdout.flush()?;
				continue;
			}
		};
		let response = json!({
			"type": "response",
			"command": command.get("type").cloned().unwrap_or(Value::Null),
			"success": false,
			"error": "rpc commands not implemented yet (pi-rs framework stage)"
		});
		writeln!(stdout, "{response}")?;
		stdout.flush()?;
	}
	Ok(())
}
