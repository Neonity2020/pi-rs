//! `/mcp` slash command round trip through a real PTY (round 45): a bare
//! `/mcp` prints the per-server status from the last connect pass and
//! `/mcp logout <server>` reports the missing credentials through a notice.
//! Skips cleanly when no PTY-capable python3 is available.

#[cfg(unix)]
#[test]
fn tui_mcp_status_and_logout_round_trip_in_a_terminal() {
	if std::process::Command::new("python3")
		.arg("--version")
		.output()
		.is_err()
	{
		eprintln!("skipping: python3 not available");
		return;
	}
	let root = std::env::temp_dir().join(format!("pi-tui-mcp-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let output = std::process::Command::new("python3")
		.arg(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/tests/support/tui_mcp_pty.py"
		))
		.arg(env!("CARGO_BIN_EXE_pi-rs"))
		.arg(&root)
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"stdout: {}\nstderr: {}",
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr)
	);
	let _ = std::fs::remove_dir_all(root);
}
