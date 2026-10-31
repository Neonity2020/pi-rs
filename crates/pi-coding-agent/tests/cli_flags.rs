//! Integration tests for the tenth-round CLI flags (`--session-id`,
//! `--fork`, `--session-dir`, `--export`, `-a`/`-na`, `--offline`,
//! `--models`) — real-subprocess style like `session_cli.rs`, temp dirs only,
//! no network and no real keys.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A throwaway root with `project/` (cwd) and `agent/` (PI_RS_AGENT_DIR).
struct Fixture {
	root: PathBuf,
	project: PathBuf,
	agent: PathBuf,
}

impl Fixture {
	fn new(tag: &str) -> Self {
		let root =
			std::env::temp_dir().join(format!("pi-cli-flags-{tag}-{}", uuid::Uuid::new_v4()));
		let project = root.join("project");
		let agent = root.join("agent");
		std::fs::create_dir_all(&project).unwrap();
		std::fs::create_dir_all(&agent).unwrap();
		Self {
			root,
			project,
			agent,
		}
	}

	fn sessions_root(&self) -> PathBuf {
		self.agent.join("sessions")
	}

	/// A session directory fixture: `<dir>/session.jsonl` with a v3 header
	/// (`id`, `cwd`) plus optional extra entry lines.
	fn session(&self, dir_name: &str, id: &str, entries: &[String]) -> PathBuf {
		let dir = self.sessions_root().join(dir_name);
		std::fs::create_dir_all(&dir).unwrap();
		let header = serde_json::json!({
			"type": "session",
			"version": 3,
			"id": id,
			"timestamp": "2026-01-01T00:00:00.000Z",
			"cwd": self.project,
		});
		let mut text = format!("{header}\n");
		for entry in entries {
			text.push_str(entry);
			text.push('\n');
		}
		std::fs::write(dir.join("session.jsonl"), text).unwrap();
		dir
	}

	/// One RPC round trip: `get_state` (or another command) in rpc mode.
	fn rpc(&self, args: &[&str], command: &str) -> (Option<i32>, serde_json::Value, String) {
		let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent)
			.env_remove("PI_RS_SESSION_DIR")
			.env_remove("PI_SESSION_DIR")
			.env_remove("PI_OFFLINE")
			.args(["--no-mcp", "--mode", "rpc"])
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		writeln!(child.stdin.take().unwrap(), "{command}").unwrap();
		let output = child.wait_with_output().unwrap();
		let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
		let frame: serde_json::Value =
			serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
		(output.status.code(), frame, stderr)
	}

	fn rpc_state(&self, args: &[&str]) -> (Option<i32>, serde_json::Value, String) {
		self.rpc(args, "{\"type\":\"get_state\"}")
	}

	/// [`Fixture::rpc_state`] with extra environment variables layered on.
	fn rpc_env_state(
		&self,
		envs: &[(&str, &str)],
		args: &[&str],
	) -> (Option<i32>, serde_json::Value, String) {
		let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent)
			.env_remove("PI_RS_SESSION_DIR")
			.env_remove("PI_SESSION_DIR")
			.env_remove("PI_OFFLINE")
			.args(["--no-mcp", "--mode", "rpc"])
			.args(args)
			.envs(envs.iter().copied())
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		writeln!(child.stdin.take().unwrap(), "{{\"type\":\"get_state\"}}").unwrap();
		let output = child.wait_with_output().unwrap();
		let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
		let frame: serde_json::Value =
			serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
		(output.status.code(), frame, stderr)
	}

	fn raw(&self, args: &[&str]) -> Output {
		Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent)
			.env_remove("PI_RS_SESSION_DIR")
			.env_remove("PI_SESSION_DIR")
			.env_remove("PI_OFFLINE")
			.args(args)
			.stdin(Stdio::null())
			.output()
			.unwrap()
	}

	/// Every session directory under the sessions root.
	fn session_dirs(&self) -> Vec<PathBuf> {
		let Ok(entries) = std::fs::read_dir(self.sessions_root()) else {
			return Vec::new();
		};
		let mut dirs: Vec<PathBuf> = entries
			.flatten()
			.map(|entry| entry.path())
			.filter(|path| path.is_dir())
			.collect();
		dirs.sort();
		dirs
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.root);
	}
}

fn header_of(dir: &Path) -> serde_json::Value {
	let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
	text.lines()
		.next()
		.map(|line| serde_json::from_str(line).unwrap())
		.unwrap()
}

/// Header-cwd equality modulo symlink spelling (macOS `/var` vs
/// `/private/var`): the child process records its canonical cwd, the fixture
/// path keeps the `/var/...` spelling.
fn assert_cwd_matches(header: &serde_json::Value, project: &Path) {
	let recorded = Path::new(header["cwd"].as_str().unwrap());
	assert_eq!(
		recorded.canonicalize().unwrap(),
		project.canonicalize().unwrap(),
		"header cwd {recorded:?} != project {project:?}"
	);
}

const MESSAGE_ENTRY: &str = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello fork","timestamp":1}}"#;

// -- --session-id -------------------------------------------------------------

#[test]
fn session_id_opens_the_existing_project_session() {
	let fixture = Fixture::new("session-id-open");
	let dir = fixture.session("20260101-old", "sess-fixed", &[]);
	let (code, frame, stderr) = fixture.rpc_state(&["--session-id", "sess-fixed"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert_eq!(
		frame["data"]["sessionFile"],
		dir.join("session.jsonl").to_string_lossy().as_ref()
	);
	// Nothing new was created for an exact match.
	assert_eq!(fixture.session_dirs().len(), 1);
}

#[test]
fn session_id_creates_a_new_session_with_that_id_and_warns() {
	let fixture = Fixture::new("session-id-create");
	let (code, _frame, stderr) = fixture.rpc_state(&["--session-id", "brand-new-id"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		stderr.contains(
			"Warning: No project session found with id 'brand-new-id'; creating a new session with that id."
		),
		"{stderr}"
	);
	let dirs = fixture.session_dirs();
	assert_eq!(dirs.len(), 1, "{:?}", dirs);
	let header = header_of(&dirs[0]);
	assert_eq!(header["id"], "brand-new-id");
	assert_cwd_matches(&header, &fixture.project);
	// A second run reopens it instead of warning again.
	let (code, frame, stderr) = fixture.rpc_state(&["--session-id", "brand-new-id"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		!stderr.contains("No project session found"),
		"reopen must not warn: {stderr}"
	);
	assert_eq!(
		frame["data"]["sessionFile"],
		dirs[0].join("session.jsonl").to_string_lossy().as_ref()
	);
}

#[test]
fn session_id_with_no_session_stays_in_memory_but_keeps_the_id() {
	let fixture = Fixture::new("session-id-memory");
	let output = fixture.raw(&[
		"--no-mcp",
		"--model",
		"mock/mock-echo",
		"--no-session",
		"--session-id",
		"ephemeral-1",
		"--mode",
		"json",
		"hi",
	]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let stdout = String::from_utf8_lossy(&output.stdout);
	let first = stdout.lines().next().expect("json header line");
	let header: serde_json::Value = serde_json::from_str(first).unwrap();
	assert_eq!(header["type"], "session");
	assert_eq!(header["id"], "ephemeral-1");
	// Nothing was persisted.
	assert!(fixture.session_dirs().is_empty());
}

#[test]
fn session_id_and_fork_flag_conflicts_exit_1() {
	let fixture = Fixture::new("conflicts");
	let cases: &[(&[&str], &str)] = &[
		(
			&["--session-id", "x", "--continue"],
			"--session-id cannot be combined with --continue",
		),
		(
			// `--session` is pi-rs's merged `--resume` flag (takes a value,
			// unlike upstream's boolean picker).
			&["--session-id", "x", "--session", "y"],
			"--session-id cannot be combined with --session",
		),
		(
			&["--fork", "x", "--continue"],
			"--fork cannot be combined with --continue",
		),
		(
			&["--fork", "x", "--no-session"],
			"--fork cannot be combined with --no-session",
		),
		(
			&["--fork", "x", "--session", "y"],
			"--fork cannot be combined with --session",
		),
	];
	for (args, expected) in cases {
		let output = fixture.raw(args);
		assert_eq!(output.status.code(), Some(1), "{args:?}");
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(stderr.contains(expected), "{args:?}: {stderr}");
	}
	// Validation happens before any session is created.
	assert!(fixture.session_dirs().is_empty());
}

#[test]
fn session_id_validates_the_id_shape() {
	let fixture = Fixture::new("session-id-shape");
	let output = fixture.raw(&["--session-id", "bad id!"]);
	assert_eq!(output.status.code(), Some(1));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains(
			"Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character"
		),
		"{stderr}"
	);
}

// -- --fork -------------------------------------------------------------------

fn fork_source() -> String {
	MESSAGE_ENTRY.to_string()
}

#[test]
fn fork_resolves_by_id_prefix_and_links_the_parent_session() {
	let fixture = Fixture::new("fork-id");
	let source = fixture.session(
		"20260101-src",
		"aaaa1111-2222-3333-4444-555566667777",
		&[fork_source()],
	);
	let (code, frame, stderr) = fixture.rpc_state(&["--fork", "aaaa1111"]);
	assert_eq!(code, Some(0), "{stderr}");
	let forked_file = frame["data"]["sessionFile"].as_str().unwrap();
	assert_ne!(
		forked_file,
		source.join("session.jsonl").to_string_lossy().as_ref()
	);
	let forked_dir = Path::new(forked_file).parent().unwrap();
	let header = header_of(forked_dir);
	// New id, current project cwd, parentSession → the source log.
	assert_ne!(header["id"], "aaaa1111-2222-3333-4444-555566667777");
	assert_cwd_matches(&header, &fixture.project);
	assert_eq!(
		header["parentSession"],
		source.join("session.jsonl").to_string_lossy().as_ref()
	);
	// The transcript path was copied.
	let text = std::fs::read_to_string(forked_file).unwrap();
	assert!(text.contains("hello fork"), "{text}");
	// The forked session reopens by its header id.
	let (code, frame, stderr) =
		fixture.rpc_state(&["--session-id", header["id"].as_str().unwrap()]);
	assert_eq!(code, Some(0), "{stderr}");
	assert_eq!(frame["data"]["sessionFile"], forked_file);
}

#[test]
fn fork_by_path_and_by_directory_name() {
	let fixture = Fixture::new("fork-path");
	let source = fixture.session(
		"20260101-src",
		"bbbb2222-2222-3333-4444-555566667777",
		&[fork_source()],
	);
	// Full path to the session directory.
	let (code, frame, stderr) = fixture.rpc_state(&["--fork", source.to_string_lossy().as_ref()]);
	assert_eq!(code, Some(0), "{stderr}");
	let forked = frame["data"]["sessionFile"].as_str().unwrap();
	assert_ne!(
		forked,
		source.join("session.jsonl").to_string_lossy().as_ref()
	);
	// Path to the log file itself.
	let (code, _frame, stderr) = fixture.rpc_state(&[
		"--fork",
		source.join("session.jsonl").to_string_lossy().as_ref(),
	]);
	assert_eq!(code, Some(0), "{stderr}");
	// Directory-name exact match.
	let (code, _frame, stderr) = fixture.rpc_state(&["--fork", "20260101-src"]);
	assert_eq!(code, Some(0), "{stderr}");
}

#[test]
fn fork_unknown_session_exits_1() {
	let fixture = Fixture::new("fork-missing");
	let output = fixture.raw(&["--no-mcp", "--mode", "rpc", "--fork", "zzzznot-there"]);
	assert_eq!(output.status.code(), Some(1));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("No session found matching 'zzzznot-there'"),
		"{stderr}"
	);
	assert!(fixture.session_dirs().is_empty());
}

#[test]
fn fork_with_session_id_dedups_and_names_the_target() {
	let fixture = Fixture::new("fork-dedup");
	let source = fixture.session("20260101-src", "cccc3333-2222-3333-4444-555566667777", &[]);
	fixture.session("20260102-target", "target-id", &[]);
	// Target exists → the upstream collision error, exit 1.
	let output = fixture.raw(&[
		"--no-mcp",
		"--mode",
		"rpc",
		"--fork",
		"cccc3333",
		"--session-id",
		"target-id",
	]);
	assert_eq!(output.status.code(), Some(1));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("Session already exists with id 'target-id'"),
		"{stderr}"
	);
	// Free target id → the fork carries it.
	let (code, frame, stderr) =
		fixture.rpc_state(&["--fork", "cccc3333", "--session-id", "fresh-target"]);
	assert_eq!(code, Some(0), "{stderr}");
	let forked_file = frame["data"]["sessionFile"].as_str().unwrap();
	let header = header_of(Path::new(forked_file).parent().unwrap());
	assert_eq!(header["id"], "fresh-target");
	assert_eq!(
		header["parentSession"],
		source.join("session.jsonl").to_string_lossy().as_ref()
	);
}

// -- --session-dir ------------------------------------------------------------

#[test]
fn session_dir_controls_storage_and_lookup() {
	let fixture = Fixture::new("session-dir");
	let custom = fixture.root.join("custom-sessions");
	let (code, frame, stderr) = fixture.rpc_state(&["--session-dir", custom.to_str().unwrap()]);
	assert_eq!(code, Some(0), "{stderr}");
	let file = frame["data"]["sessionFile"].as_str().unwrap();
	assert!(file.starts_with(custom.to_str().unwrap()), "{file}");
	// Default root stayed empty.
	assert!(fixture.session_dirs().is_empty());
	// `--continue` finds the session through the same directory.
	let (code, frame, stderr) =
		fixture.rpc_state(&["--session-dir", custom.to_str().unwrap(), "--continue"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert_eq!(frame["data"]["sessionFile"], file);
}

#[test]
fn session_dir_env_vars_apply_and_the_flag_wins() {
	let fixture = Fixture::new("session-dir-env");
	let from_env = fixture.root.join("env-sessions");
	// The pi-rs env var.
	let (code, frame, stderr) =
		fixture.rpc_env_state(&[("PI_RS_SESSION_DIR", from_env.to_str().unwrap())], &[]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		frame["data"]["sessionFile"]
			.as_str()
			.unwrap()
			.starts_with(from_env.to_str().unwrap()),
		"{}",
		frame["data"]["sessionFile"]
	);
	// The upstream-name env var.
	let from_upstream_env = fixture.root.join("upstream-env-sessions");
	let (code, frame, stderr) = fixture.rpc_env_state(
		&[("PI_SESSION_DIR", from_upstream_env.to_str().unwrap())],
		&[],
	);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		frame["data"]["sessionFile"]
			.as_str()
			.unwrap()
			.starts_with(from_upstream_env.to_str().unwrap()),
		"{}",
		frame["data"]["sessionFile"]
	);
	// The CLI flag beats the env.
	let from_flag = fixture.root.join("flag-sessions");
	let (code, frame, stderr) = fixture.rpc_env_state(
		&[("PI_SESSION_DIR", from_upstream_env.to_str().unwrap())],
		&["--session-dir", from_flag.to_str().unwrap()],
	);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		frame["data"]["sessionFile"]
			.as_str()
			.unwrap()
			.starts_with(from_flag.to_str().unwrap()),
		"{}",
		frame["data"]["sessionFile"]
	);
}

// -- --export -----------------------------------------------------------------

#[test]
fn export_writes_an_html_file_and_exits() {
	let fixture = Fixture::new("export");
	fixture.session(
		"20260101-src",
		"dddd4444-2222-3333-4444-555566667777",
		&[MESSAGE_ENTRY.to_string()],
	);
	let out = fixture.root.join("exported.html");
	let output = fixture.raw(&["--no-mcp", "--export", "dddd4444", out.to_str().unwrap()]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let stdout = String::from_utf8_lossy(&output.stdout);
	assert!(
		stdout.contains(&format!("Exported to: {}", out.display())),
		"{stdout}"
	);
	let html = std::fs::read_to_string(&out).unwrap();
	assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
	assert!(html.contains("hello fork"), "{html}");
}

#[test]
fn export_default_output_name_derives_from_the_session() {
	let fixture = Fixture::new("export-default");
	let source = fixture.session(
		"20260101-src",
		"eeee5555-2222-3333-4444-555566667777",
		&[MESSAGE_ENTRY.to_string()],
	);
	let output = fixture.raw(&["--no-mcp", "--export", "20260101-src"]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let expected = fixture.project.join("pi-rs-session-20260101-src.html");
	assert!(
		expected.exists(),
		"{}",
		String::from_utf8_lossy(&output.stdout)
	);
	// A `.jsonl` path argument strips the extension like upstream.
	let output = fixture.raw(&[
		"--no-mcp",
		"--export",
		source.join("session.jsonl").to_str().unwrap(),
	]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let from_file = fixture.project.join("pi-rs-session-session.html");
	assert!(
		from_file.exists(),
		"{}",
		String::from_utf8_lossy(&output.stdout)
	);
}

#[test]
fn export_failure_prints_error_and_exits_1() {
	let fixture = Fixture::new("export-fail");
	let output = fixture.raw(&["--no-mcp", "--export", "ffffff-not-there"]);
	assert_eq!(output.status.code(), Some(1));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(stderr.contains("Error:"), "{stderr}");
	assert!(
		stderr.contains("No session found matching 'ffffff-not-there'"),
		"{stderr}"
	);
}

// -- -a / -na (projectTrustOverride) ------------------------------------------

fn trust_fixture(tag: &str) -> (Fixture, PathBuf) {
	let fixture = Fixture::new(tag);
	let marker = fixture.root.join("mcp-ran");
	let config = serde_json::json!({"mcpServers": {"t": {
		"command": "sh",
		"args": ["-c", format!("touch '{}'; exit 0", marker.display())],
	}}});
	std::fs::create_dir_all(fixture.project.join(".pi")).unwrap();
	std::fs::write(fixture.project.join(".pi/mcp.json"), config.to_string()).unwrap();
	(fixture, marker)
}

#[test]
fn untrusted_print_mode_skips_project_mcp_servers() {
	let (fixture, marker) = trust_fixture("trust-skip");
	let output = fixture.raw(&["--model", "mock/mock-echo", "--no-session", "hi"]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("exists but the project is not trusted; project MCP servers are skipped"),
		"{stderr}"
	);
	assert!(!marker.exists(), "project MCP server must not run");
}

#[test]
fn approve_runs_project_mcp_servers_without_prompting() {
	let (fixture, marker) = trust_fixture("trust-approve");
	let output = fixture.raw(&["-a", "--model", "mock/mock-echo", "--no-session", "hi"]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(!stderr.contains("not trusted"), "{stderr}");
	assert!(marker.exists(), "project MCP server must run under -a");
	// The override never persists a trust decision.
	assert!(!fixture.agent.join("trust.json").exists());
}

#[test]
fn no_approve_skips_project_mcp_servers_without_prompting() {
	let (fixture, marker) = trust_fixture("trust-no-approve");
	let output = fixture.raw(&[
		"--no-approve",
		"--model",
		"mock/mock-echo",
		"--no-session",
		"hi",
	]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("(--no-approve)"),
		"override notice expected: {stderr}"
	);
	assert!(
		!marker.exists(),
		"project MCP server must not run under -na"
	);
	// The short form behaves the same.
	let (fixture, marker) = trust_fixture("trust-na-short");
	let output = fixture.raw(&["-na", "--model", "mock/mock-echo", "--no-session", "hi"]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(!marker.exists());
}

// -- --models -----------------------------------------------------------------

#[test]
fn models_scope_shapes_rpc_model_listing_and_default() {
	let fixture = Fixture::new("models-scope");
	let (code, frame, stderr) = fixture.rpc_state(&["--models", "mock/*"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert_eq!(frame["data"]["model"]["provider"], "mock");
	assert_eq!(frame["data"]["model"]["id"], "mock-echo");
	let (_code, models, _stderr) = fixture.rpc(
		&["--models", "mock/*"],
		"{\"type\":\"get_available_models\"}",
	);
	assert_eq!(models["success"], true);
	let list = models["data"]["models"].as_array().unwrap();
	assert!(!list.is_empty());
	assert!(
		list.iter().all(|model| model["provider"] == "mock"),
		"{list:?}"
	);
	// Scope with an explicit thinking suffix selects the level.
	let (_code, state, _stderr) = fixture.rpc_state(&["--models", "gpt-5.2:high"]);
	assert_eq!(state["data"]["model"]["id"], "gpt-5.2");
	assert_eq!(state["data"]["thinkingLevel"], "high");
}

#[test]
fn models_no_match_warns_to_stderr() {
	let fixture = Fixture::new("models-warn");
	let (code, _frame, stderr) = fixture.rpc_state(&["--models", "does-not-exist*"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		stderr.contains("Warning: No models match pattern \"does-not-exist*\""),
		"{stderr}"
	);
}

// -- --offline ----------------------------------------------------------------

#[test]
fn offline_flag_starts_normally() {
	let fixture = Fixture::new("offline");
	let (code, frame, stderr) = fixture.rpc_state(&["--offline"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert_eq!(frame["success"], true);
}
