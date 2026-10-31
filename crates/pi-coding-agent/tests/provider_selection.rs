use std::process::{Command, Stdio};

#[test]
fn provider_option_and_settings_disambiguate_model_ids() {
	let dir = std::env::temp_dir().join(format!("pi-provider-select-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	let models = serde_json::json!({"providers": {
		"first": {"api":"mock", "models":[{"id":"shared"}]},
		"second": {"api":"mock", "models":[{"id":"shared"}]}
	}});
	std::fs::write(dir.join("models.json"), models.to_string()).unwrap();
	std::fs::write(
		dir.join("settings.json"),
		r#"{"defaultProvider":"second","defaultModel":"shared"}"#,
	)
	.unwrap();
	let run = |extra: &[&str]| {
		Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&dir)
			.env("PI_RS_AGENT_DIR", &dir)
			.stdin(Stdio::null())
			.args(["--no-mcp", "--no-session", "--mode", "json"])
			.args(extra)
			.arg("hello")
			.output()
			.unwrap()
	};
	for args in [vec![], vec!["--provider", "second", "--model", "shared"]] {
		let output = run(&args);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		let events = String::from_utf8(output.stdout).unwrap();
		assert!(
			events
				.lines()
				.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
				.any(|event| {
					event["type"] == "message_end"
						&& event["message"]["role"] == "assistant"
						&& event["message"]["provider"] == "second"
				}),
			"provider not selected: {events}"
		);
	}
	let invalid = run(&["--provider", "unknown", "--model", "shared"]);
	assert!(!invalid.status.success());
	for (args, expected) in [
		(vec!["--provider", "second", "--model", "shared:low"], "low"),
		(
			vec![
				"--provider",
				"second",
				"--model",
				"shared:low",
				"--thinking",
				"high",
			],
			"high",
		),
		(
			vec![
				"--provider",
				"second",
				"--model",
				"shared:low",
				"--thinking",
				"invalid",
			],
			"low",
		),
	] {
		let output = run(&args);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		let events = String::from_utf8(output.stdout).unwrap();
		assert!(
			events
				.lines()
				.filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
				.any(|event| {
					event["type"] == "message_end"
						&& event["message"]["role"] == "assistant"
						&& event["message"]["thinkingLevel"] == expected
				}),
			"thinking level not set: {events}"
		);
		if args.contains(&"invalid") {
			assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid thinking level"));
		}
	}
	std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn list_models_filters_without_starting_mcp() {
	let dir = std::env::temp_dir().join(format!("pi-model-list-{}", uuid::Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	std::fs::write(dir.join("mcp.json"), "invalid JSON").unwrap();
	let run = |args: &[&str]| {
		Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&dir)
			.env("PI_RS_AGENT_DIR", &dir)
			.stdin(Stdio::null())
			.args(args)
			.output()
			.unwrap()
	};
	let all = run(&["--list-models"]);
	assert!(all.status.success());
	let output = String::from_utf8(all.stdout).unwrap();
	assert!(
		output.contains("provider")
			&& output.contains("mimo-v2.6-pro")
			&& output.contains("mock-echo")
	);
	assert!(all.stderr.is_empty(), "MCP should not be loaded");
	let filtered = run(&["--list-models", "mimo"]);
	let output = String::from_utf8(filtered.stdout).unwrap();
	assert!(output.contains("mimo-v2.6-pro"));
	assert!(!output.contains("mock-echo"));
	let missing = run(&["--list-models", "zzzzzzzzz"]);
	assert!(
		String::from_utf8(missing.stdout)
			.unwrap()
			.contains("No models matching")
	);
	std::fs::remove_dir_all(dir).unwrap();
}
