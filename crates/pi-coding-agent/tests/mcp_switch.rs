#[cfg(unix)]
#[test]
fn cross_project_switch_reconnects_only_trusted_servers_and_respects_no_mcp() {
	use std::io::Write;
	use std::process::{Command, Stdio};

	let root = std::env::temp_dir().join(format!("pi-mcp-switch-{}", uuid::Uuid::new_v4()));
	let agent = root.join("agent");
	let projects: Vec<_> = ["a", "b", "untrusted"]
		.iter()
		.map(|name| root.join(name))
		.collect();
	std::fs::create_dir_all(&agent).unwrap();
	let script = root.join("server.py");
	std::fs::write(&script, r#"import json, os, sys
marker = sys.argv[1]
with open(marker, 'a') as f: f.write('start:' + os.getcwd() + '\n')
try:
    for line in sys.stdin:
        message = json.loads(line)
        if 'id' not in message: continue
        if message.get('method') == 'initialize':
            result = {'protocolVersion':'2025-06-18','capabilities':{},'serverInfo':{'name':'test','version':'1'}}
        elif message.get('method') == 'tools/list':
            result = {'tools':[{'name':'echo','description':'test','inputSchema':{'type':'object'}}]}
        else: result = {}
        print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
finally:
    with open(marker, 'a') as f: f.write('stop\n')
"#).unwrap();
	for project in &projects {
		std::fs::create_dir_all(project.join(".pi")).unwrap();
		let config = serde_json::json!({"mcpServers":{"test":{"command":"python3","args":[script, project.join("marker")]}}});
		std::fs::write(project.join(".pi/mcp.json"), config.to_string()).unwrap();
	}
	let trusted_a = projects[0].canonicalize().unwrap();
	let trusted_b = projects[1].canonicalize().unwrap();
	let trust =
		serde_json::json!({trusted_a.to_str().unwrap():true, trusted_b.to_str().unwrap():true});
	std::fs::write(agent.join("trust.json"), trust.to_string()).unwrap();
	let targets: Vec<_> = projects[1..].iter().enumerate().map(|(i, project)| {
		let dir = root.join(format!("session-{i}"));
		std::fs::create_dir_all(&dir).unwrap();
		let header = serde_json::json!({"type":"session","version":3,"id":format!("test-{i}"),"timestamp":"2026-01-01T00:00:00.000Z","cwd":project});
		std::fs::write(dir.join("session.jsonl"), format!("{header}\n")).unwrap();
		dir
	}).collect();
	for disable in [false, true] {
		for project in &projects {
			let _ = std::fs::remove_file(project.join("marker"));
		}
		let mut command = Command::new(env!("CARGO_BIN_EXE_pi-rs"));
		command
			.current_dir(&projects[0])
			.env("PI_RS_AGENT_DIR", &agent)
			.env_remove("PI_RS_SESSION_DIR")
			.args(["--mode", "rpc"])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		if disable {
			command.arg("--no-mcp");
		}
		let mut child = command.spawn().unwrap();
		let mut stdin = child.stdin.take().unwrap();
		for (id, target) in targets.iter().enumerate() {
			writeln!(
				stdin,
				"{}",
				serde_json::json!({"type":"switch_session", "id":id, "sessionPath":target})
			)
			.unwrap();
		}
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
		for id in 0..2 {
			assert!(
				frames
					.iter()
					.any(|frame| frame["id"] == id && frame["success"] == true),
				"{frames:?}"
			);
		}
		if disable {
			assert!(
				projects
					.iter()
					.all(|project| !project.join("marker").exists())
			);
		} else {
			for project in &projects[..2] {
				let marker = std::fs::read_to_string(project.join("marker")).unwrap();
				assert!(
					marker.contains(&format!(
						"start:{}",
						project.canonicalize().unwrap().display()
					)),
					"{marker}"
				);
				assert!(marker.contains("stop"), "{marker}");
			}
			assert!(!projects[2].join("marker").exists());
		}
	}
	std::fs::remove_dir_all(root).unwrap();
}
