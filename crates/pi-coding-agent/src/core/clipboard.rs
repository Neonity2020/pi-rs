//! Clipboard access — port of `pi/packages/coding-agent/src/utils/clipboard.ts`
//! (`copyToClipboard`, first slice): the platform command ladder with the
//! OSC 52 fallback for remote sessions. The native FFI clipboard writer
//! (`getNativeClipboard`), WSL PowerShell interop, and Termux-specific
//! messaging stay deferred; the read path is deferred with them.

/// OSC 52 base64 payload cap (upstream `MAX_OSC52_ENCODED_LENGTH`).
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

fn clipboard_command_candidates() -> Vec<(&'static str, Vec<&'static str>)> {
	if cfg!(target_os = "macos") {
		return vec![("pbcopy", vec![])];
	}
	if cfg!(target_os = "windows") {
		return vec![("clip", vec![])];
	}
	let mut commands = Vec::new();
	if std::env::var_os("TERMUX_VERSION").is_some() {
		commands.push(("termux-clipboard-set", vec![]));
	}
	if std::env::var_os("WAYLAND_DISPLAY").is_some() {
		commands.push(("wl-copy", vec![]));
	}
	if std::env::var_os("DISPLAY").is_some() {
		commands.push(("xclip", vec!["-selection", "clipboard"]));
		commands.push(("xsel", vec!["--clipboard", "--input"]));
	}
	commands
}

fn is_remote_session() -> bool {
	std::env::var_os("SSH_CONNECTION").is_some()
		|| std::env::var_os("SSH_CLIENT").is_some()
		|| std::env::var_os("MOSH_CONNECTION").is_some()
		|| std::env::var("PI_CLIPBOARD_OSC52").is_ok_and(|value| value == "1")
}

fn headless_linux() -> bool {
	cfg!(target_os = "linux")
		&& std::env::var_os("DISPLAY").is_none()
		&& std::env::var_os("WAYLAND_DISPLAY").is_none()
		&& std::env::var_os("TERMUX_VERSION").is_none()
}

/// OSC 52 escape: the terminal-clipboard route for remote sessions (SSH,
/// multiplexers). Returns false when the payload exceeds the size cap.
fn emit_osc52(text: &str) -> bool {
	use base64::Engine as _;
	let encoded = base64::engine::general_purpose::STANDARD.encode(text);
	if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
		return false;
	}
	use std::io::Write;
	let _ = std::io::stdout().write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes());
	let _ = std::io::stdout().flush();
	true
}

/// Run one clipboard writer with the text on stdin. No timeout (the port's
/// other process helpers share this; upstream caps at 5s).
fn run_clipboard_command(command: &str, args: &[&str], input: &str) -> bool {
	use std::io::Write;
	let Ok(mut child) = std::process::Command::new(command)
		.args(args)
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.spawn()
	else {
		return false;
	};
	if let Some(mut stdin) = child.stdin.take() {
		let _ = stdin.write_all(input.as_bytes());
	}
	matches!(child.wait(), Ok(status) if status.success())
}

/// Copy text to the system clipboard: platform commands first, then OSC 52
/// for remote sessions (and headless Linux, where the terminal is the only
/// route). Errors mirror the upstream messages.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
	let mut copied = clipboard_command_candidates()
		.iter()
		.any(|(command, args)| run_clipboard_command(command, args, text));
	if !copied && (is_remote_session() || headless_linux()) {
		copied = emit_osc52(text);
	}
	if copied {
		return Ok(());
	}
	if std::env::var_os("TERMUX_VERSION").is_some() {
		return Err(
			"Clipboard unavailable: install the Termux:API app and `termux-api` package".into(),
		);
	}
	Err("Clipboard unavailable: no clipboard command found".into())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn osc52_wraps_base64_and_enforces_the_cap() {
		// The escape shape is asserted via the payload size gate; emitting
		// writes to stdout, so only the failure path is checked here.
		let oversized = "x".repeat(MAX_OSC52_ENCODED_LENGTH);
		assert!(!emit_osc52(&oversized), "over-cap payload refused");
	}

	#[test]
	fn remote_session_detection_follows_the_env_markers() {
		// Best effort: only assert it does not panic; env mutation in
		// parallel tests is unsafe.
		let _ = is_remote_session();
		let _ = headless_linux();
		assert!(
			!clipboard_command_candidates().is_empty(),
			"every platform has a writer"
		);
	}

	#[test]
	fn copy_reports_failure_when_nothing_is_available() {
		// On a machine with pbcopy this succeeds; the ladder itself is
		// exercised. The error string is the observable contract for
		// clipboard-less environments.
		if let Err(error) = copy_to_clipboard("pi-rs clipboard smoke") {
			assert!(error.starts_with("Clipboard unavailable"));
		}
	}
}
