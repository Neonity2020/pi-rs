//! Extensions end-to-end through the real binary: `-e` entry loading, the
//! trust gate on project `.pi/extensions/`, `-ne` discovery semantics,
//! `tool_call` interception in json/print mode (hasUI=false), extension
//! commands on the prompt path, failure isolation, and teardown without
//! leaked host processes. Model is the in-process mock (`mock/mock-echo`),
//! whose "list" trigger produces the `ls` tool call the block fixture
//! intercepts — the mock cannot call custom tools, so the extension-tool
//! execute loop is covered at assembly level (core/extensions.rs tests) and
//! in the round-13 PoC.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Absolute path of an extension fixture.
fn fixture(name: &str) -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests")
		.join("fixtures")
		.join("extensions")
		.join(name)
}

struct Fixture {
	root: PathBuf,
	project: PathBuf,
	agent_dir: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let root = std::env::temp_dir().join(format!(
			"pi-ext-cli-{}-{}",
			std::process::id(),
			uuid::Uuid::new_v4().simple()
		));
		let project = root.join("project");
		let agent_dir = root.join("agent");
		std::fs::create_dir_all(&project).unwrap();
		std::fs::create_dir_all(&agent_dir).unwrap();
		Self {
			root,
			project,
			agent_dir,
		}
	}

	/// Install `fixture_name` as a project extension (`.pi/extensions/`).
	fn project_extension(&self, fixture_name: &str) {
		let dir = self.project.join(".pi/extensions");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::copy(fixture(fixture_name), dir.join(fixture_name)).unwrap();
	}

	/// Run pi-rs in json mode and collect (stdout, stderr); asserts success.
	fn run(&self, extra_args: &[&str], prompt: &str) -> (String, String) {
		let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent_dir)
			.env("PI_RS_SESSION_DIR", self.root.join("sessions"))
			.env_remove("PI_OFFLINE")
			.args([
				"--no-mcp",
				"--mode",
				"json",
				"--no-session",
				"--model",
				"mock/mock-echo",
				// The mock provider's tool-call trigger names `ls`, which is
				// not in the default active set (read/bash/edit/write).
				"--tools",
				"+ls",
			])
			.args(extra_args)
			.arg(prompt)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
			.wait_with_output()
			.unwrap();
		assert!(
			output.status.success(),
			"exit {:?}\nstdout:\n{}\nstderr:\n{}",
			output.status.code(),
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr)
		);
		(
			String::from_utf8_lossy(&output.stdout).into_owned(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	}

	/// json run with a persisted session (`--session-dir` under the root,
	/// no `--no-session`) for assertions on the session log.
	fn run_with_session(&self, extra_args: &[&str], prompt: &str) -> (String, String) {
		let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent_dir)
			.env("PI_RS_SESSION_DIR", self.root.join("sessions"))
			.args([
				"--no-mcp",
				"--mode",
				"json",
				"--model",
				"mock/mock-echo",
				"--session-dir",
				self.root.join("sessions").to_str().unwrap(),
			])
			.args(extra_args)
			.arg(prompt)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
			.wait_with_output()
			.unwrap();
		assert!(output.status.success(), "exit {:?}", output.status.code());
		(
			String::from_utf8_lossy(&output.stdout).into_owned(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	}

	/// Print-mode (text) run for the hasUI=false semantics.
	fn run_print_text(&self, extra_args: &[&str], prompt: &str) -> (String, String) {
		let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent_dir)
			.env("PI_RS_SESSION_DIR", self.root.join("sessions"))
			.args([
				"--no-mcp",
				"-p",
				"--no-session",
				"--model",
				"mock/mock-echo",
				"--tools",
				"+ls",
			])
			.args(extra_args)
			.arg(prompt)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
			.wait_with_output()
			.unwrap();
		assert!(output.status.success(), "exit {:?}", output.status.code());
		(
			String::from_utf8_lossy(&output.stdout).into_owned(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.root);
	}
}

fn frames(stdout: &str) -> Vec<serde_json::Value> {
	stdout
		.lines()
		.filter(|line| !line.trim().is_empty())
		.map(|line| serde_json::from_str(line).expect("each stdout line is JSON"))
		.collect()
}

fn node_available() -> bool {
	std::process::Command::new("node")
		.arg("--version")
		.output()
		.is_ok()
}

fn require_node() -> bool {
	if node_available() {
		true
	} else {
		eprintln!("skipping: node not available");
		false
	}
}

/// `-e <file>` + a prompt that triggers `ls`: the tool_call interception
/// blocks the call in json mode (hasUI=false — the fixture's own default
/// block reason), and the blocked toolResult reaches the model.
#[test]
fn tool_call_interception_blocks_ls_in_json_mode() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	let (stdout, stderr) = fx.run(
		&["-e", fixture("block-ls.ts").to_str().unwrap()],
		"list files",
	);
	// The interception ran: ls was called and blocked with the fixture's
	// no-UI reason (hasUI=false in json mode).
	let all = frames(&stdout);
	let ls_calls: Vec<&serde_json::Value> = all
		.iter()
		.filter(|frame| frame["type"] == "tool_execution_end" && frame["toolName"] == "ls")
		.collect();
	assert_eq!(ls_calls.len(), 1, "{stdout}");
	assert_eq!(ls_calls[0]["isError"], serde_json::json!(true));
	assert!(
		serde_json::to_string(&ls_calls[0]["result"])
			.unwrap()
			.contains("ls blocked by extension (no UI)"),
		"{stdout}"
	);
	assert!(!stderr.contains("extension"), "{stderr}");
	// The run settles normally after the blocked call.
	assert_eq!(all.last().unwrap()["type"], "agent_settled");
}

/// Print text mode: same hasUI=false blocking, visible in the final reply.
#[test]
fn permission_gate_blocks_in_print_mode_without_ui() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	let (stdout, _stderr) = fx.run_print_text(
		&["-e", fixture("block-ls.ts").to_str().unwrap()],
		"list files",
	);
	assert!(
		stdout.contains("ls blocked by extension (no UI)"),
		"{stdout}"
	);
}

/// Without the extension the same prompt lets `ls` through (the mock
/// summarizes the real listing, not the block error).
#[test]
fn without_extension_ls_is_not_blocked() {
	let fx = Fixture::new();
	let (stdout, _stderr) = fx.run(&[], "list files");
	assert!(!stdout.contains("ls blocked by extension"), "{stdout}");
}

/// `-ne` skips the discovered directories but explicit `-e` paths still load
/// (upstream resource-loader semantics).
#[test]
fn no_extensions_skips_discovery_but_keeps_explicit_paths() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	fx.project_extension("block-ls.ts");

	// `-ne` alone: the project extension does not load, and no trust notice
	// appears even though the project is untrusted.
	let (stdout, stderr) = fx.run(&["--no-extensions"], "list files");
	assert!(!stdout.contains("ls blocked by extension"), "{stdout}");
	assert!(!stderr.contains("not trusted"), "{stderr}");

	// `-ne -e <file>`: the explicit path still loads and blocks.
	let (stdout, stderr) = fx.run(
		&[
			"--no-extensions",
			"-e",
			fixture("block-ls.ts").to_str().unwrap(),
		],
		"list files",
	);
	assert!(
		stdout.contains("ls blocked by extension (no UI)"),
		"{stdout}"
	);
	assert!(!stderr.contains("not trusted"), "{stderr}");
}

/// The trust gate: an untrusted project skips `.pi/extensions/` with a
/// notice; `-a` (projectTrustOverride) loads it without prompting.
#[test]
fn project_extensions_respect_the_trust_gate() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	fx.project_extension("block-ls.ts");

	// Non-interactive + untrusted: skipped with a notice, ls not blocked.
	let (stdout, stderr) = fx.run(&[], "list files");
	assert!(!stdout.contains("ls blocked by extension"), "{stdout}");
	assert!(
		stderr.contains("exists but the project is not trusted; project extensions are skipped"),
		"{stderr}"
	);

	// `-a`: the override trusts the project for this run.
	let (stdout, stderr) = fx.run(&["-a"], "list files");
	assert!(
		stdout.contains("ls blocked by extension (no UI)"),
		"{stdout}"
	);
	assert!(!stderr.contains("not trusted"), "{stderr}");

	// `-na`: untrusted even with local resources.
	let (stdout, stderr) = fx.run(&["-na"], "list files");
	assert!(!stdout.contains("ls blocked by extension"), "{stdout}");
	assert!(
		stderr.contains("project extensions are skipped"),
		"{stderr}"
	);
}

/// A failing factory is isolated into a notice; the run continues.
#[test]
fn a_broken_extension_degrades_to_a_notice() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	let (stdout, stderr) = fx.run(
		&["-e", fixture("factory-throw.ts").to_str().unwrap()],
		"just echo",
	);
	// The echo reply still streams (the failure did not take the run down).
	let all = frames(&stdout);
	assert_eq!(all.last().unwrap()["type"], "agent_settled");
	assert!(stdout.contains("just echo"), "{stdout}");
	// The extension error surfaces as a startup notice with its own message.
	assert!(stderr.contains("factory exploded on purpose"), "{stderr}");
	assert!(stderr.contains("factory-throw.ts"), "{stderr}");
}

/// `/session-name` executes through the prompt path (upstream `prompt()`
/// runs extension commands before everything else): the notify dialog lands
/// as a notice and the sync `setSessionName` action is applied to the live
/// session log.
#[test]
fn extension_command_executes_on_the_prompt_path() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	let session_dir = fx.root.join("sessions");
	let (stdout, stderr) = fx.run_with_session(
		&["-e", fixture("session-name.ts").to_str().unwrap()],
		"/session-name Named By Extension",
	);
	assert!(
		stderr.contains("Session named: Named By Extension"),
		"stderr: {stderr}\nstdout: {stdout}"
	);
	// The settle boundary fires for the command-only prompt (no run).
	assert_eq!(frames(&stdout).last().unwrap()["type"], "agent_settled");
	// The action-frame application wrote the session_info entry.
	let sessions = list_session_files(&session_dir);
	assert_eq!(sessions.len(), 1, "one session created");
	let log = std::fs::read_to_string(&sessions[0]).unwrap();
	assert!(log.contains("Named By Extension"), "{log}");
	assert!(log.contains("session_info"), "{log}");
}

/// A prompt starting with `/` that matches no extension command still works
/// (falls through to the ordinary message path).
#[test]
fn an_unknown_slash_prompt_passes_through() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	let (stdout, stderr) = fx.run(
		&["-e", fixture("session-name.ts").to_str().unwrap()],
		"/not-a-command hello",
	);
	assert!(stdout.contains("/not-a-command hello"), "{stdout}");
	assert!(!stderr.contains("unknown extension command"), "{stderr}");
}

/// Teardown leaves no host processes behind.
#[test]
fn teardown_does_not_leak_host_processes() {
	if !require_node() {
		return;
	}
	let fx = Fixture::new();
	let (_stdout, _stderr) = fx.run(&["-e", fixture("hello.ts").to_str().unwrap()], "just echo");
	// Give the reaper a moment, then assert no bridge host remains: the
	// host script path carries the pi-ext-host workdir prefix.
	std::thread::sleep(std::time::Duration::from_millis(500));
	let leaked = Command::new("pgrep")
		.args(["-f", "pi-ext-host-.*host\\.mjs"])
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.output()
		.unwrap();
	assert!(
		leaked.stdout.is_empty(),
		"leaked host processes: {}",
		String::from_utf8_lossy(&leaked.stdout)
	);
}

fn list_session_files(root: &Path) -> Vec<PathBuf> {
	let mut found = Vec::new();
	let Ok(entries) = std::fs::read_dir(root) else {
		return found;
	};
	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_dir() {
			let log = path.join("session.jsonl");
			if log.is_file() {
				found.push(log);
			}
		}
	}
	found
}
