//! Interactive extension dialog round trip through a real PTY (round 42):
//! `/ask` opens the extension's `ui.select` as a TUI dialog, Enter answers it
//! through the bridge registry, and the handler's notify surfaces the choice.
//! Skips cleanly when no PTY-capable python3 is available.

#[cfg(unix)]
#[test]
fn tui_extension_dialog_round_trips_in_a_terminal() {
	if std::process::Command::new("node")
		.arg("--version")
		.output()
		.is_err()
	{
		eprintln!("skipping: node not available");
		return;
	}
	let root = std::env::temp_dir().join(format!("pi-tui-ext-dialog-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let output = std::process::Command::new("python3")
		.arg(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/tests/support/tui_extension_dialog_pty.py"
		))
		.arg(env!("CARGO_BIN_EXE_pi-rs"))
		.arg(&root)
		.arg(crate_fixture())
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

#[cfg(unix)]
fn crate_fixture() -> String {
	std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures/extensions/dialogs.ts")
		.display()
		.to_string()
}
