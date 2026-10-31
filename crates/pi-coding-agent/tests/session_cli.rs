use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn session_name_and_session_alias_survive_restart() {
	let dir = std::env::temp_dir().join(format!("pi-session-cli-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	let run = |args: &[&str]| {
		let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&dir)
			.env("PI_RS_AGENT_DIR", &dir)
			.env_remove("PI_RS_SESSION_DIR")
			.args(["--no-mcp", "--mode", "rpc"])
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		writeln!(child.stdin.take().unwrap(), "{{\"type\":\"get_state\"}}").unwrap();
		let output = child.wait_with_output().unwrap();
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["data"].clone()
	};
	let created = run(&["-n", "first name"]);
	assert_eq!(created["sessionName"], "first name");
	let file = created["sessionFile"].as_str().unwrap();
	let resumed = run(&["--session", file, "--name", "updated name"]);
	assert_eq!(resumed["sessionFile"], file);
	assert_eq!(resumed["sessionName"], "updated name");
	let reopened = run(&["--session", file]);
	assert_eq!(reopened["sessionName"], "updated name");
	let memory = run(&["--no-session", "--name", "temporary"]);
	assert_eq!(memory["sessionName"], "temporary");
	assert!(memory["sessionFile"].is_null());
	let invalid = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&dir)
		.env("PI_RS_AGENT_DIR", &dir)
		.args(["--name", "   "])
		.output()
		.unwrap();
	assert!(!invalid.status.success());
	assert!(String::from_utf8_lossy(&invalid.stderr).contains("--name requires a non-empty value"));
	std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resumed_session_uses_its_project_settings() {
	let root = std::env::temp_dir().join(format!("pi-session-cwd-{}", uuid::Uuid::new_v4()));
	let project = root.join("project");
	let other = root.join("other");
	let agent = root.join("agent");
	std::fs::create_dir_all(project.join(".pi")).unwrap();
	std::fs::create_dir_all(&other).unwrap();
	std::fs::create_dir_all(&agent).unwrap();
	std::fs::write(project.join("project-marker.txt"), "target").unwrap();
	std::fs::write(other.join("other-marker.txt"), "other").unwrap();
	std::fs::write(
		project.join(".pi/settings.json"),
		r#"{"defaultModel":"mock/mock-echo"}"#,
	)
	.unwrap();
	let run = |cwd: &std::path::Path, args: &[&str]| {
		let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(cwd)
			.env("PI_RS_AGENT_DIR", &agent)
			.env_remove("PI_RS_SESSION_DIR")
			.args(["--no-mcp", "--mode", "rpc"])
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		writeln!(child.stdin.take().unwrap(), "{{\"type\":\"get_state\"}}").unwrap();
		child.wait_with_output().unwrap()
	};
	let first = run(&project, &[]);
	assert!(
		first.status.success(),
		"{}",
		String::from_utf8_lossy(&first.stderr)
	);
	let state: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
	let file = state["data"]["sessionFile"].as_str().unwrap();
	let resumed = run(&other, &["--session", file]);
	assert!(
		resumed.status.success(),
		"{}",
		String::from_utf8_lossy(&resumed.stderr)
	);
	let state: serde_json::Value = serde_json::from_slice(&resumed.stdout).unwrap();
	assert_eq!(state["data"]["model"]["id"], "mock-echo");
	let tools = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&other)
		.env("PI_RS_AGENT_DIR", &agent)
		.env_remove("PI_RS_SESSION_DIR")
		.args([
			"--no-mcp",
			"--session",
			file,
			"--tools",
			"ls",
			"-p",
			"list files",
		])
		.stdin(Stdio::null())
		.output()
		.unwrap();
	assert!(
		tools.status.success(),
		"{}",
		String::from_utf8_lossy(&tools.stderr)
	);
	let output = String::from_utf8_lossy(&tools.stdout);
	assert!(output.contains("project-marker.txt"), "{output}");
	assert!(!output.contains("other-marker.txt"), "{output}");
	std::fs::remove_dir_all(&project).unwrap();
	let missing = run(&other, &["--session", file]);
	assert!(!missing.status.success());
	assert!(
		String::from_utf8_lossy(&missing.stderr)
			.contains("session working directory does not exist")
	);
	std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn rpc_waits_for_replacement_before_next_command_and_eof() {
	let dir = std::env::temp_dir().join(format!("pi-rpc-replace-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&dir)
		.env("PI_RS_AGENT_DIR", &dir)
		.env_remove("PI_RS_SESSION_DIR")
		.args(["--no-mcp", "--mode", "rpc"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let mut stdin = child.stdin.take().unwrap();
	writeln!(stdin, "{{\"type\":\"new_session\",\"id\":\"new\"}}").unwrap();
	writeln!(stdin, "{{\"type\":\"get_state\",\"id\":\"state\"}}").unwrap();
	drop(stdin);
	let output = child.wait_with_output().unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let frames: Vec<serde_json::Value> = String::from_utf8(output.stdout)
		.unwrap()
		.lines()
		.filter_map(|line| serde_json::from_str(line).ok())
		.collect();
	let new = frames
		.iter()
		.position(|frame| frame["id"] == "new")
		.unwrap();
	let state = frames
		.iter()
		.position(|frame| frame["id"] == "state")
		.unwrap();
	assert!(new < state);
	assert_eq!(frames[new]["success"], true);
	assert_eq!(frames[state]["success"], true);
	std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn continue_reopens_most_recently_modified_session_in_current_project() {
	let root = std::env::temp_dir().join(format!("pi-continue-{}", uuid::Uuid::new_v4()));
	let project = root.join("project");
	let agent = root.join("agent");
	std::fs::create_dir_all(&project).unwrap();
	for (id, cwd, modified) in [
		("20260101-old", project.clone(), 3),
		("20261231-new", project.clone(), 2),
		("20270101-other", root.clone(), 4),
	] {
		let dir = agent.join("sessions").join(id);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("session.jsonl");
		let header = serde_json::json!({"type":"session", "version":3, "id":id, "timestamp":"2026-01-01T00:00:00Z", "cwd":cwd});
		std::fs::write(&path, format!("{header}\n")).unwrap();
		std::fs::File::open(&path)
			.unwrap()
			.set_modified(
				std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(modified),
			)
			.unwrap();
	}
	let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&project)
		.env("PI_RS_AGENT_DIR", &agent)
		.env_remove("PI_RS_SESSION_DIR")
		.args(["--no-mcp", "--mode", "rpc", "--continue"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	writeln!(child.stdin.take().unwrap(), "{{\"type\":\"get_state\"}}").unwrap();
	let output = child.wait_with_output().unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let frame: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
	assert_eq!(
		frame["data"]["sessionFile"],
		agent
			.join("sessions/20260101-old/session.jsonl")
			.to_string_lossy()
			.as_ref()
	);
	std::fs::remove_dir_all(root).unwrap();
}
