//! Interactive ui.* decoration round trip through a real PTY (round 43):
//! `/decorate` fires `ui.setStatus`/`ui.setTitle`/`ui.setEditorText` without
//! awaiting them; the footer grows a status line, the editor buffer is
//! replaced, and the terminal window title is set. Skips cleanly when no
//! PTY-capable python3 is available.

#[cfg(unix)]
#[test]
fn tui_extension_decorations_round_trip_in_a_terminal() {
	if std::process::Command::new("node")
		.arg("--version")
		.output()
		.is_err()
	{
		eprintln!("skipping: node not available");
		return;
	}
	let root = std::env::temp_dir().join(format!("pi-tui-ext-deco-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let output = std::process::Command::new("python3")
		.arg(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/tests/support/tui_extension_decorate_pty.py"
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
		.join("tests/fixtures/extensions/ui-decorations.ts")
		.display()
		.to_string()
}
