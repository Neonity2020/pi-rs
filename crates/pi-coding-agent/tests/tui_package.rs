//! Package resource startup through a real PTY (round 47): a settings
//! `packages` entry's extension registers a command that answers through a
//! notify — the resolve-into-startup wiring works end to end. Skips cleanly
//! when no PTY-capable python3 is available.

#[cfg(unix)]
#[test]
fn tui_package_extension_command_round_trips_in_a_terminal() {
	if std::process::Command::new("python3")
		.arg("--version")
		.output()
		.is_err()
	{
		eprintln!("skipping: python3 not available");
		return;
	}
	let root = std::env::temp_dir().join(format!("pi-tui-pkg-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let output = std::process::Command::new("python3")
		.arg(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/tests/support/tui_package_pty.py"
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
