//! Open a URL or file in the platform browser/default handler — port of
//! `pi/packages/coding-agent/src/utils/open-browser.ts`.
//!
//! This intentionally never invokes a shell. On Windows, do not use
//! `cmd /c start`: cmd.exe re-parses metacharacters (&, |, ^, ...) before
//! `start` runs, which would make attacker-controlled URLs injectable.

/// Launch the platform handler for `target`. Best-effort: a missing launcher
/// (for example `xdg-open`) is ignored — callers still present the target to
/// the user (upstream swallows the spawn `error` event).
pub fn open_browser(target: &str) {
	#[cfg(target_os = "macos")]
	let (command, args) = ("open", vec![target.to_string()]);
	#[cfg(windows)]
	let (command, args) = (
		"rundll32",
		vec![
			"url.dll,FileProtocolHandler".to_string(),
			target.to_string(),
		],
	);
	#[cfg(not(any(target_os = "macos", windows)))]
	let (command, args) = ("xdg-open", vec![target.to_string()]);
	let _ = std::process::Command::new(command)
		.args(&args)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.spawn();
}

#[cfg(test)]
mod tests {
	#[test]
	fn open_browser_never_panics_and_never_shells_out() {
		// A URL with shell metacharacters must not reach a shell; the
		// launcher itself may be missing (CI), which is ignored.
		super::open_browser("https://example.com/?a=1&b=|rm");
	}
}
