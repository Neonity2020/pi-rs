#[cfg(unix)]
#[test]
fn tui_session_commands_work_in_a_terminal() {
	let root = std::env::temp_dir().join(format!("pi-tui-cmds-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let output = std::process::Command::new("python3")
		.arg(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/tests/support/tui_session_cmds_pty.py"
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
	std::fs::remove_dir_all(root).unwrap();
}
