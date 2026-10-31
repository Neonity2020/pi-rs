#[cfg(unix)]
#[test]
fn no_mcp_skips_config_resolution_and_server_startup() {
	use std::process::{Command, Stdio};

	let dir = std::env::temp_dir().join(format!("pi-no-mcp-{}", uuid::Uuid::new_v4()));
	let agent = dir.join("agent");
	std::fs::create_dir_all(&agent).unwrap();
	let resolved = dir.join("resolved");
	let started = dir.join("started");
	let config = serde_json::json!({"mcpServers": {"test": {
		"command": "sh",
		"args": ["-c", format!("touch '{}'; exit 0", started.display())],
		"env": {"TEST_VALUE": format!("!touch '{}'; printf ok", resolved.display())}
	}}});
	std::fs::write(agent.join("mcp.json"), config.to_string()).unwrap();
	let run = |disable: bool| {
		let mut command = Command::new(env!("CARGO_BIN_EXE_pi-rs"));
		command
			.current_dir(&dir)
			.env("PI_RS_AGENT_DIR", &agent)
			.stdin(Stdio::null())
			.args(["--no-session", "--print", "--model", "mock/mock-echo"]);
		if disable {
			command.arg("--no-mcp");
		}
		command.arg("hello").output().unwrap()
	};
	let disabled = run(true);
	assert!(
		disabled.status.success(),
		"{}",
		String::from_utf8_lossy(&disabled.stderr)
	);
	assert!(
		!resolved.exists(),
		"MCP config command ran despite --no-mcp"
	);
	assert!(!started.exists(), "MCP server started despite --no-mcp");
	let enabled = run(false);
	assert!(
		enabled.status.success(),
		"{}",
		String::from_utf8_lossy(&enabled.stderr)
	);
	assert!(resolved.exists(), "control: config command did not run");
	assert!(started.exists(), "control: MCP server did not start");
	std::fs::remove_dir_all(dir).unwrap();
}
