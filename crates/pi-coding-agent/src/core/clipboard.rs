//! Clipboard access — port of `pi/packages/coding-agent/src/utils/clipboard.ts`
//! (`copyToClipboard`): the platform command ladder (5s timeout per writer),
//! OSC 52 for remote sessions and headless Linux (100k base64 cap), the WSL
//! PowerShell interop, and the upstream error messages. The read path
//! (`paste_from_clipboard`) ports `clipboard-image.ts` and `custom-editor.ts`:
//! file paths (macOS Finder copy) -> image (saved to /tmp/pi-clipboard-*.png) ->
//! text fallback.

use std::path::PathBuf;

/// Content retrieved from system clipboard on paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteContent {
	FilePaths(Vec<String>),
	ImageFile(PathBuf),
	Text(String),
	Empty,
}

/// OSC 52 base64 payload cap (upstream `MAX_OSC52_ENCODED_LENGTH`).
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;
/// Per-writer cap (upstream `runClipboardCommand` default `timeoutMs: 5000`).
const COMMAND_TIMEOUT_MS: u64 = 5_000;

/// Platform command ladder for direct writes — upstream `copyToClipboard`'s
/// command table (Termux reports platform "android", so `TERMUX_VERSION`
/// gates it on Linux; Wayland/X11 probes pick the Linux writers).
fn clipboard_command_candidates() -> Vec<(&'static str, Vec<&'static str>)> {
	if cfg!(target_os = "macos") {
		return vec![("pbcopy", vec![])];
	}
	if cfg!(target_os = "windows") {
		return vec![("clip", vec![])];
	}
	let mut commands = Vec::new();
	if env_set("TERMUX_VERSION") {
		commands.push(("termux-clipboard-set", vec![]));
	}
	if env_set("WAYLAND_DISPLAY") {
		commands.push(("wl-copy", vec![]));
	}
	if env_set("DISPLAY") {
		commands.push(("xclip", vec!["-selection", "clipboard"]));
		commands.push(("xsel", vec!["--clipboard", "--input"]));
	}
	commands
}

fn env_set(name: &str) -> bool {
	std::env::var_os(name).is_some()
}

/// Remote sessions also emit OSC 52 so the client terminal receives the copy.
/// `PI_CLIPBOARD_OSC52=1` covers remote clients that leave no SSH markers,
/// such as multiplexer attaches or IDE daemons (upstream `isRemoteSession`).
fn is_remote_session() -> bool {
	env_set("SSH_CONNECTION")
		|| env_set("SSH_CLIENT")
		|| env_set("MOSH_CONNECTION")
		|| std::env::var("PI_CLIPBOARD_OSC52").is_ok_and(|value| value == "1")
}

/// Upstream `isWSL`: `WSL_DISTRO_NAME`/`WSLENV` markers, else the
/// `/proc/version` release string.
fn is_wsl() -> bool {
	if env_set("WSL_DISTRO_NAME") || env_set("WSLENV") {
		return true;
	}
	std::fs::read_to_string("/proc/version")
		.map(|release| {
			release.to_lowercase().contains("microsoft") || release.to_lowercase().contains("wsl")
		})
		.unwrap_or(false)
}

/// Upstream's headless rule: Linux with no display at all — the terminal is
/// the only clipboard route (containers, WSL without WSLg).
fn headless_linux() -> bool {
	cfg!(target_os = "linux")
		&& !env_set("DISPLAY")
		&& !env_set("WAYLAND_DISPLAY")
		&& !env_set("TERMUX_VERSION")
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

/// Run one clipboard writer with the text on stdin, killing it at the 5s cap
/// (upstream `runClipboardCommand` timeout; a hung `xclip` must not freeze
/// the TUI).
fn run_clipboard_command(command: &str, args: &[&str], input: &str) -> bool {
	use std::io::Write;
	use std::time::{Duration, Instant};
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
		// Dropping stdin closes the pipe — `xclip`/`xsel` read to EOF.
	}
	let deadline = Instant::now() + Duration::from_millis(COMMAND_TIMEOUT_MS);
	let status = loop {
		match child.try_wait() {
			Ok(Some(status)) => break Some(status),
			Ok(None) if Instant::now() >= deadline => {
				let _ = child.kill();
				let _ = child.wait();
				return false;
			}
			Ok(None) => std::thread::sleep(Duration::from_millis(10)),
			Err(_) => return false,
		}
	};
	status.is_some_and(|status| status.success())
}

/// WSL without WSLg has no Linux display, so the Windows clipboard is written
/// through interop. PowerShell reads the text from a file because `clip.exe`
/// and PowerShell stdin decode piped bytes with the console code page, which
/// mangles non-ASCII UTF-8 (upstream `copyViaWindowsClipboard`).
fn copy_via_windows_clipboard(text: &str) -> bool {
	let tmp_file =
		std::env::temp_dir().join(format!("pi-rs-wsl-clip-{}.txt", uuid::Uuid::new_v4()));
	let cleanup = || {
		let _ = std::fs::remove_file(&tmp_file);
	};
	if std::fs::write(&tmp_file, text).is_err() {
		cleanup();
		return false;
	}
	let Some(win_path) =
		run_clipboard_command_capture("wslpath", ["-w", &tmp_file.to_string_lossy()])
	else {
		cleanup();
		return false;
	};
	let win_path = win_path.trim();
	if win_path.is_empty() {
		cleanup();
		return false;
	}
	let script = format!(
		"Set-Clipboard -Value ([System.IO.File]::ReadAllText('{}', [System.Text.Encoding]::UTF8))",
		win_path.replace('\'', "''")
	);
	let copied = run_clipboard_command("powershell.exe", &["-NoProfile", "-Command", &script], "");
	cleanup();
	copied
}

/// One-shot capture variant for `wslpath` (stdout wanted, 1s cap upstream).
fn run_clipboard_command_capture(command: &str, args: [&str; 2]) -> Option<String> {
	use std::time::{Duration, Instant};
	let mut child = std::process::Command::new(command)
		.args(args)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::null())
		.spawn()
		.ok()?;
	let deadline = Instant::now() + Duration::from_secs(1);
	let status = loop {
		match child.try_wait() {
			Ok(Some(status)) => break status,
			Ok(None) if Instant::now() >= deadline => {
				let _ = child.kill();
				let _ = child.wait();
				return None;
			}
			Ok(None) => std::thread::sleep(Duration::from_millis(10)),
			Err(_) => return None,
		}
	};
	if !status.success() {
		return None;
	}
	let mut stdout = String::new();
	if let Some(mut pipe) = child.stdout.take() {
		use std::io::Read;
		let _ = pipe.read_to_string(&mut stdout);
	}
	Some(stdout)
}

/// Copy text to the system clipboard: platform commands first (they retain
/// selection ownership after this returns), then OSC 52 for remote sessions
/// and headless Linux where the terminal is the only route. Errors mirror the
/// upstream messages.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
	let mut copied = clipboard_command_candidates()
		.iter()
		.any(|(command, args)| run_clipboard_command(command, args, text));
	let mut osc52_emitted = false;
	if !copied && cfg!(target_os = "linux") && is_wsl() {
		// Windows Terminal supports OSC 52; prefer it over the slower
		// PowerShell round trip.
		if env_set("WT_SESSION") {
			osc52_emitted = emit_osc52(text);
		}
		copied = osc52_emitted || copy_via_windows_clipboard(text);
	}
	// OSC 52 cannot be verified, so a desktop session with a display reports
	// the failure instead; without a display the terminal is the only
	// clipboard route, and remote sessions always emit it to reach the
	// client clipboard.
	let mut oversized = false;
	if !osc52_emitted && (is_remote_session() || (!copied && headless_linux())) {
		if emit_osc52(text) {
			copied = true;
		} else {
			oversized = true;
		}
	}
	if copied {
		return Ok(());
	}
	if oversized {
		return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".into());
	}
	if env_set("TERMUX_VERSION") {
		return Err(
			"Clipboard unavailable: install the Termux:API app and `termux-api` package".into(),
		);
	}
	if cfg!(target_os = "linux") {
		if env_set("WAYLAND_DISPLAY") {
			return Err(
				"Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
					.into(),
			);
		}
		if env_set("DISPLAY") {
			return Err(
				"Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".into(),
			);
		}
	}
	Err("Clipboard unavailable".into())
}

fn run_clipboard_command_capture_bytes(
	command: &str,
	args: &[&str],
	timeout_ms: u64,
) -> Option<Vec<u8>> {
	use std::time::{Duration, Instant};
	let mut child = std::process::Command::new(command)
		.args(args)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::null())
		.spawn()
		.ok()?;
	let deadline = Instant::now() + Duration::from_millis(timeout_ms);
	let status = loop {
		match child.try_wait() {
			Ok(Some(status)) => break status,
			Ok(None) if Instant::now() >= deadline => {
				let _ = child.kill();
				let _ = child.wait();
				return None;
			}
			Ok(None) => std::thread::sleep(Duration::from_millis(10)),
			Err(_) => return None,
		}
	};
	if !status.success() {
		return None;
	}
	let mut stdout = Vec::new();
	if let Some(mut pipe) = child.stdout.take() {
		use std::io::Read;
		let _ = pipe.read_to_end(&mut stdout);
	}
	Some(stdout)
}

/// Read file paths from clipboard (macOS Finder copy).
pub fn read_clipboard_file_paths() -> Option<Vec<String>> {
	if cfg!(target_os = "macos") {
		let script = "set theList to {}\ntry\nrepeat with anItem in (the clipboard as list)\ntry\nset end of theList to POSIX path of (anItem as «class furl»)\nend try\nend repeat\nend try\nset AppleScript's text item delimiters to linefeed\ntheList as text";
		let bytes = run_clipboard_command_capture_bytes("osascript", &["-e", script], 2000)?;
		let text = String::from_utf8_lossy(&bytes);
		let paths: Vec<String> = text
			.lines()
			.map(|line| line.trim().to_string())
			.filter(|line| !line.is_empty())
			.collect();
		if !paths.is_empty() {
			// Upstream check: throw if path contains control characters (\p{Cc})
			if paths.iter().any(|p| p.chars().any(|c| c.is_control())) {
				return None;
			}
			return Some(paths);
		}
	}
	None
}

/// Read image bytes from system clipboard and write to temporary PNG file.
pub fn read_clipboard_image() -> Option<PathBuf> {
	if env_set("TERMUX_VERSION") {
		return None;
	}

	let tmp_dir = std::env::temp_dir();
	let tmp_file = tmp_dir.join(format!("pi-clipboard-{}.png", uuid::Uuid::new_v4()));

	if cfg!(target_os = "macos") {
		let path_str = tmp_file.to_string_lossy().to_string();
		let script = format!(
			"set theFile to open for access POSIX file \"{path_str}\" with write permission\n\
				 set eof theFile to 0\n\
				 set ok to false\n\
				 try\n\
				     write (the clipboard as «class PNGf») to theFile\n\
				     set ok to true\n\
				 on error\n\
				     try\n\
				         write (the clipboard as «class TIFF») to theFile\n\
				         set ok to true\n\
				     end try\n\
				 end try\n\
				 close access theFile\n\
				 if ok then\n\
				     return \"ok\"\n\
				 else\n\
				     do shell script \"rm -f \" & quoted form of \"{path_str}\"\n\
				     return \"empty\"\n\
				 end if"
		);
		let out =
			run_clipboard_command_capture_bytes("osascript", &["-e", &script], COMMAND_TIMEOUT_MS)?;
		let out_str = String::from_utf8_lossy(&out).trim().to_string();
		if out_str != "ok" || !tmp_file.exists() {
			return None;
		}

		// Check if written file is TIFF and convert to PNG if so
		if let Ok(bytes) = std::fs::read(&tmp_file)
			&& bytes.len() >= 4
			&& (bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"))
			&& let Ok(img) = image::load_from_memory(&bytes)
		{
			let _ = img.save_with_format(&tmp_file, image::ImageFormat::Png);
		}
		if std::fs::metadata(&tmp_file).is_ok_and(|m| m.len() > 0) {
			return Some(tmp_file);
		}
		let _ = std::fs::remove_file(&tmp_file);
		return None;
	}

	if cfg!(target_os = "linux") {
		if (env_set("WAYLAND_DISPLAY") || is_wsl())
			&& let Some(list_bytes) =
				run_clipboard_command_capture_bytes("wl-paste", &["--list-types"], 1000)
		{
			let list_str = String::from_utf8_lossy(&list_bytes);
			let preferred = ["image/png", "image/jpeg", "image/webp", "image/gif"];
			let mut chosen_type = None;
			for pref in preferred {
				if list_str
					.lines()
					.any(|l| l.trim().eq_ignore_ascii_case(pref))
				{
					chosen_type = Some(pref);
					break;
				}
			}
			if chosen_type.is_none() {
				chosen_type = list_str.lines().find(|l| l.trim().starts_with("image/"));
			}
			if let Some(mime) = chosen_type
				&& let Some(data) = run_clipboard_command_capture_bytes(
					"wl-paste",
					&["--type", mime.trim(), "--no-newline"],
					COMMAND_TIMEOUT_MS,
				) && !data.is_empty()
				&& std::fs::write(&tmp_file, &data).is_ok()
			{
				return Some(tmp_file);
			}
		}

		if env_set("DISPLAY")
			&& let Some(targets_bytes) = run_clipboard_command_capture_bytes(
				"xclip",
				&["-selection", "clipboard", "-t", "TARGETS", "-o"],
				1000,
			) {
			let targets_str = String::from_utf8_lossy(&targets_bytes);
			let preferred = ["image/png", "image/jpeg", "image/webp", "image/gif"];
			let mut chosen_type = None;
			for pref in preferred {
				if targets_str
					.lines()
					.any(|l| l.trim().eq_ignore_ascii_case(pref))
				{
					chosen_type = Some(pref);
					break;
				}
			}
			if chosen_type.is_none() {
				chosen_type = targets_str.lines().find(|l| l.trim().starts_with("image/"));
			}
			if let Some(mime) = chosen_type
				&& let Some(data) = run_clipboard_command_capture_bytes(
					"xclip",
					&["-selection", "clipboard", "-t", mime.trim(), "-o"],
					COMMAND_TIMEOUT_MS,
				) && !data.is_empty()
				&& std::fs::write(&tmp_file, &data).is_ok()
			{
				return Some(tmp_file);
			}
		}

		if is_wsl()
			&& let Some(win_path) =
				run_clipboard_command_capture("wslpath", ["-w", &tmp_file.to_string_lossy()])
		{
			let win_path = win_path.trim();
			if !win_path.is_empty() {
				let script = format!(
					"Add-Type -AssemblyName System.Windows.Forms; \
						 Add-Type -AssemblyName System.Drawing; \
						 $img = [System.Windows.Forms.Clipboard]::GetImage(); \
						 if ($img) {{ $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png); 'ok' }} else {{ 'empty' }}",
					win_path.replace('\'', "''")
				);
				if let Some(res) = run_clipboard_command_capture_bytes(
					"powershell.exe",
					&["-NoProfile", "-Command", &script],
					COMMAND_TIMEOUT_MS,
				) && String::from_utf8_lossy(&res).trim() == "ok"
					&& tmp_file.exists()
					&& std::fs::metadata(&tmp_file).is_ok_and(|m| m.len() > 0)
				{
					return Some(tmp_file);
				}
			}
		}
	}

	if cfg!(target_os = "windows") {
		let path_str = tmp_file.to_string_lossy().to_string();
		let script = format!(
			"Add-Type -AssemblyName System.Windows.Forms; \
				 Add-Type -AssemblyName System.Drawing; \
				 $img = [System.Windows.Forms.Clipboard]::GetImage(); \
				 if ($img) {{ $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png); 'ok' }} else {{ 'empty' }}",
			path_str.replace('\'', "''")
		);
		if let Some(res) = run_clipboard_command_capture_bytes(
			"powershell.exe",
			&["-NoProfile", "-Command", &script],
			COMMAND_TIMEOUT_MS,
		) && String::from_utf8_lossy(&res).trim() == "ok"
			&& tmp_file.exists()
			&& std::fs::metadata(&tmp_file).is_ok_and(|m| m.len() > 0)
		{
			return Some(tmp_file);
		}
	}

	None
}

/// Read text from system clipboard using platform commands.
pub fn read_clipboard_text() -> Option<String> {
	if cfg!(target_os = "macos") {
		let bytes = run_clipboard_command_capture_bytes("pbpaste", &[], COMMAND_TIMEOUT_MS)?;
		let text = String::from_utf8_lossy(&bytes).to_string();
		return Some(text);
	}
	if cfg!(target_os = "windows") {
		let bytes = run_clipboard_command_capture_bytes(
			"powershell.exe",
			&["-NoProfile", "-Command", "Get-Clipboard"],
			COMMAND_TIMEOUT_MS,
		)?;
		let text = String::from_utf8_lossy(&bytes).to_string();
		return Some(text);
	}
	if cfg!(target_os = "linux") {
		if env_set("WAYLAND_DISPLAY")
			&& let Some(bytes) = run_clipboard_command_capture_bytes(
				"wl-paste",
				&["--no-newline"],
				COMMAND_TIMEOUT_MS,
			) {
			return Some(String::from_utf8_lossy(&bytes).to_string());
		}
		if env_set("DISPLAY") {
			if let Some(bytes) = run_clipboard_command_capture_bytes(
				"xclip",
				&["-selection", "clipboard", "-o"],
				COMMAND_TIMEOUT_MS,
			) {
				return Some(String::from_utf8_lossy(&bytes).to_string());
			}
			if let Some(bytes) = run_clipboard_command_capture_bytes(
				"xsel",
				&["--clipboard", "--output"],
				COMMAND_TIMEOUT_MS,
			) {
				return Some(String::from_utf8_lossy(&bytes).to_string());
			}
		}
		if is_wsl()
			&& let Some(bytes) = run_clipboard_command_capture_bytes(
				"powershell.exe",
				&["-NoProfile", "-Command", "Get-Clipboard"],
				COMMAND_TIMEOUT_MS,
			) {
			return Some(String::from_utf8_lossy(&bytes).to_string());
		}
	}
	None
}

/// Read clipboard content matching upstream `handleClipboardPaste`:
/// 1. File paths (macOS Finder copy)
/// 2. Image (saved to temp file)
/// 3. Text fallback
pub fn paste_from_clipboard() -> Result<PasteContent, String> {
	if let Some(paths) = read_clipboard_file_paths() {
		return Ok(PasteContent::FilePaths(paths));
	}
	if let Some(img_path) = read_clipboard_image() {
		return Ok(PasteContent::ImageFile(img_path));
	}
	if let Some(text) = read_clipboard_text()
		&& !text.is_empty()
	{
		return Ok(PasteContent::Text(text));
	}
	Ok(PasteContent::Empty)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn osc52_refuses_payloads_over_the_cap() {
		// One encoded byte per input byte for ASCII: a payload whose base64
		// exceeds 100_000 characters is refused without touching stdout.
		let oversized = "x".repeat(MAX_OSC52_ENCODED_LENGTH);
		assert!(!emit_osc52(&oversized), "over-cap payload refused");
	}

	#[test]
	fn osc52_accepts_payloads_under_the_cap() {
		// The escape writes to stdout; in tests it lands in the captured
		// output and is harmless. A small payload must pass the gate.
		assert!(emit_osc52("pi-rs"));
	}

	#[test]
	fn remote_session_detection_follows_the_env_markers() {
		// Best effort: only assert it does not panic; env mutation in
		// parallel tests is unsafe.
		let _ = is_remote_session();
		let _ = headless_linux();
		let _ = is_wsl();
		// macOS/Windows always have a native writer; headless Linux (no
		// DISPLAY/WAYLAND/TERMUX) legitimately has none — OSC 52 is the
		// writer there, so an empty candidate list is valid.
		if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
			assert!(
				!clipboard_command_candidates().is_empty(),
				"macOS and Windows always have a native writer"
			);
		}
	}

	#[test]
	fn copy_reports_upstream_error_messages_when_unavailable() {
		// On a machine with pbcopy this succeeds; the ladder itself is
		// exercised and the error strings are the observable contract for
		// clipboard-less environments.
		if let Err(error) = copy_to_clipboard("pi-rs clipboard smoke") {
			assert!(error.starts_with("Clipboard unavailable"), "{error}");
		}
	}

	#[test]
	fn wsl_path_helper_survives_a_missing_tool() {
		// `wslpath` does not exist on macOS/Linux CI; the helper must return
		// None instead of panicking.
		assert_eq!(
			run_clipboard_command_capture("wslpath", ["-w", "/tmp"]),
			None
		);
	}

	#[test]
	fn paste_smoke_does_not_panic() {
		// paste_from_clipboard handles empty/missing clipboard gracefully without panicking
		let _ = paste_from_clipboard();
		let _ = read_clipboard_file_paths();
		let _ = read_clipboard_image();
		let _ = read_clipboard_text();
	}
}
