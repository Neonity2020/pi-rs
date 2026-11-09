//! `pi-rs config` selector through a real PTY (round 49): the grouped list
//! renders, space toggles a resource off (persisting a `-` pattern into the
//! settings entry), and Esc exits 0. Skips cleanly when no PTY-capable
//! python3 is available.

#[cfg(unix)]
#[test]
fn tui_config_selector_round_trips_in_a_terminal() {
	if std::process::Command::new("python3")
		.arg("--version")
		.output()
		.is_err()
	{
		eprintln!("skipping: python3 not available");
		return;
	}
	let root = std::env::temp_dir().join(format!("pi-tui-config-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let output = std::process::Command::new("python3")
		.arg(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/tests/support/tui_config_pty.py"
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
