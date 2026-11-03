//! Integration tests for the valueless `-r`/`--resume` session picker and the
//! cross-project fork prompt (`main.ts:391-430` port) — real-subprocess style
//! like `cli_flags.rs`, temp dirs only, no network and no real keys.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A throwaway root with two project dirs and `agent/` (PI_RS_AGENT_DIR).
struct Fixture {
	root: PathBuf,
	project: PathBuf,
	other_project: PathBuf,
	agent: PathBuf,
}

impl Fixture {
	fn new(tag: &str) -> Self {
		let root =
			std::env::temp_dir().join(format!("pi-resume-picker-{tag}-{}", uuid::Uuid::new_v4()));
		let project = root.join("project");
		let other_project = root.join("other");
		let agent = root.join("agent");
		std::fs::create_dir_all(&project).unwrap();
		std::fs::create_dir_all(&other_project).unwrap();
		std::fs::create_dir_all(&agent).unwrap();
		Self {
			root,
			project,
			other_project,
			agent,
		}
	}

	fn sessions_root(&self) -> PathBuf {
		self.agent.join("sessions")
	}

	/// A session directory fixture with a v3 header and one user message;
	/// header id and timestamps are parameters so listing order (last message
	/// activity, newest first) is deterministic.
	fn session(&self, dir_name: &str, id: &str, cwd: &Path, ts: &str, message: &str) -> PathBuf {
		let dir = self.sessions_root().join(dir_name);
		std::fs::create_dir_all(&dir).unwrap();
		let header = serde_json::json!({
			"type": "session",
			"version": 3,
			"id": id,
			"timestamp": ts,
			"cwd": cwd,
		});
		let entry = serde_json::json!({
			"type": "message",
			"id": format!("{id}-m1"),
			"parentId": null,
			"timestamp": ts,
			"message": {"role": "user", "content": message, "timestamp": 1},
		});
		std::fs::write(dir.join("session.jsonl"), format!("{header}\n{entry}\n")).unwrap();
		dir
	}

	/// Run the binary with piped stdin (`input` is written, then closed) and
	/// return (exit code, stdout, stderr).
	fn run(&self, cwd: &Path, input: &str, args: &[&str]) -> (Option<i32>, String, String) {
		let mut child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(cwd)
			.env("PI_RS_AGENT_DIR", &self.agent)
			.env_remove("PI_RS_SESSION_DIR")
			.env_remove("PI_SESSION_DIR")
			.env_remove("PI_OFFLINE")
			.args(["--no-mcp"])
			.args(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		let mut stdin = child.stdin.take().unwrap();
		let input = input.to_string();
		std::thread::spawn(move || {
			let _ = stdin.write_all(input.as_bytes());
		});
		let output = child.wait_with_output().unwrap();
		(
			output.status.code(),
			String::from_utf8_lossy(&output.stdout).into_owned(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	}

	/// Run with stdin closed from the start (EOF).
	fn run_eof(&self, cwd: &Path, args: &[&str]) -> (Option<i32>, String, String) {
		let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(cwd)
			.env("PI_RS_AGENT_DIR", &self.agent)
			.env_remove("PI_RS_SESSION_DIR")
			.env_remove("PI_SESSION_DIR")
			.env_remove("PI_OFFLINE")
			.args(["--no-mcp"])
			.args(args)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.output()
			.unwrap();
		(
			output.status.code(),
			String::from_utf8_lossy(&output.stdout).into_owned(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	}

	/// One RPC round trip (`get_state`), tolerating non-JSON lines (the fork
	/// prompt prints to stdout before the rpc stream starts). `input` feeds
	/// the prompt/first stdin reads; the `get_state` frame follows it.
	fn rpc_state(
		&self,
		cwd: &Path,
		input: &str,
		args: &[&str],
	) -> (Option<i32>, serde_json::Value, String) {
		let full_input = format!("{input}{{\"type\":\"get_state\"}}\n");
		let (code, stdout, stderr) = self.run(
			cwd,
			&full_input,
			&["--mode", "rpc"]
				.iter()
				.chain(args.iter())
				.copied()
				.collect::<Vec<_>>(),
		);
		let frame = stdout
			.lines()
			.filter_map(|line| {
				serde_json::from_str::<serde_json::Value>(line)
					.ok()
					// The fork prompt writes "[y/N] " without a newline, so a
					// following frame can share the line — retry from `{`.
					.or_else(|| {
						line.find('{').and_then(|start| {
							serde_json::from_str::<serde_json::Value>(&line[start..]).ok()
						})
					})
			})
			.find(|value| value.get("type").is_some())
			.unwrap_or(serde_json::Value::Null);
		(code, frame, stderr)
	}

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

/// Two sessions in `project`: "newer" (message ts 2026-01-02) lists first,
/// "older" (2026-01-01) second.
fn picker_fixture(tag: &str) -> (Fixture, PathBuf, PathBuf) {
	let fixture = Fixture::new(tag);
	let older = fixture.session(
		"20260101-older",
		"sess-older",
		&fixture.project,
		"2026-01-01T00:00:00.000Z",
		"older session content",
	);
	let newer = fixture.session(
		"20260102-newer",
		"sess-newer",
		&fixture.project,
		"2026-01-02T00:00:00.000Z",
		"newer session content",
	);
	(fixture, older, newer)
}

// -- -r / valueless --resume picker -------------------------------------------

#[test]
fn r_flag_lists_and_resumes_by_number() {
	let (fixture, older, newer) = picker_fixture("number");
	// Row 1 = newest (sess-newer), resumed through `--mode json`: the first
	// stdout line is the reopened session's header.
	let (code, stdout, stderr) = fixture.run(&fixture.project, "1\n", &["--mode", "json", "-r"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		stderr.contains("Resume Session (Current Folder)"),
		"{stderr}"
	);
	let header: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
	assert_eq!(header["type"], "session");
	assert_eq!(header["id"], "sess-newer");
	assert_eq!(
		Path::new(header["cwd"].as_str().unwrap())
			.canonicalize()
			.unwrap(),
		fixture.project.canonicalize().unwrap()
	);
	drop(newer);
	let _ = older;
}

#[test]
fn r_flag_resumes_second_row_and_valueless_long_form() {
	let (fixture, older, newer) = picker_fixture("second");
	// `--resume` without a value behaves like `-r`; row 2 = sess-older.
	let (code, stdout, stderr) =
		fixture.run(&fixture.project, "2\n", &["--mode", "json", "--resume"]);
	assert_eq!(code, Some(0), "{stderr}");
	let header: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
	assert_eq!(header["id"], "sess-older");
	let _ = newer;
	assert_eq!(header_of(&older)["id"], "sess-older");
}

#[test]
fn r_flag_filters_by_text_then_selects() {
	let (fixture, older, _newer) = picker_fixture("filter");
	// A non-number line narrows the list; the following number picks from the
	// narrowed list.
	let (code, stdout, stderr) =
		fixture.run(&fixture.project, "older\n1\n", &["--mode", "json", "-r"]);
	assert_eq!(code, Some(0), "{stderr}");
	let header: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
	assert_eq!(header["id"], "sess-older");
	let _ = older;
}

#[test]
fn r_flag_empty_input_cancels_with_exit_zero() {
	let (fixture, _older, _newer) = picker_fixture("cancel");
	let (code, stdout, stderr) = fixture.run(&fixture.project, "\n", &["--mode", "json", "-r"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(stdout.contains("No session selected"), "{stdout}");
	// EOF counts as cancel too (upstream readline-EOF semantics).
	let (code, stdout, _stderr) = fixture.run_eof(&fixture.project, &["--mode", "json", "-r"]);
	assert_eq!(code, Some(0));
	assert!(stdout.contains("No session selected"), "{stdout}");
}

#[test]
fn r_flag_in_rpc_mode_errors_out() {
	let (fixture, _older, _newer) = picker_fixture("rpc");
	let (code, _stdout, stderr) = fixture.run(
		&fixture.project,
		"{\"type\":\"get_state\"}\n",
		&["--mode", "rpc", "-r"],
	);
	assert_eq!(code, Some(1));
	assert!(
		stderr.contains("--resume session picker is not available in RPC mode"),
		"{stderr}"
	);
}

#[test]
fn r_flag_conflicts_report_upstream_flag_names() {
	let (fixture, _older, _newer) = picker_fixture("conflict");
	let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&fixture.project)
		.env("PI_RS_AGENT_DIR", &fixture.agent)
		.args(["--no-mcp", "--fork", "x", "-r"])
		.output()
		.unwrap();
	assert!(!output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("--fork cannot be combined with --resume"),
		"{stderr}"
	);
	let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&fixture.project)
		.env("PI_RS_AGENT_DIR", &fixture.agent)
		.args(["--no-mcp", "--session-id", "x", "--session", "y"])
		.output()
		.unwrap();
	assert!(!output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("--session-id cannot be combined with --session"),
		"{stderr}"
	);
}

// -- cross-project fork prompt --------------------------------------------------

/// A session in `project` (id `cross-1`); resumed from `other_project` by id
/// it is a "global" hit.
fn fork_fixture(tag: &str) -> (Fixture, PathBuf) {
	let fixture = Fixture::new(tag);
	let dir = fixture.session(
		"20260103-cross",
		"cross-1",
		&fixture.project,
		"2026-01-03T00:00:00.000Z",
		"cross project content",
	);
	(fixture, dir)
}

#[test]
fn cross_project_session_id_forks_after_confirmation() {
	let (fixture, source) = fork_fixture("fork-yes");
	// `y` forks into the current project (upstream main.ts:399-407): a new
	// session with the current cwd and the source lineage opens here.
	let (code, frame, stderr) =
		fixture.rpc_state(&fixture.other_project, "y\n", &["--session", "cross-1"]);
	assert_eq!(code, Some(0), "{stderr}");
	let dirs = fixture.session_dirs();
	assert_eq!(dirs.len(), 2, "fork creates a second session: {:?}", dirs);
	let forked = dirs.iter().find(|dir| dir != &&source).expect("forked dir");
	assert_eq!(
		frame["data"]["sessionFile"],
		forked.join("session.jsonl").to_string_lossy().as_ref()
	);
	let header = header_of(forked);
	assert!(
		header["cwd"].as_str().unwrap().ends_with("other"),
		"fork header points at the current project: {}",
		header["cwd"]
	);
	assert_eq!(
		header["parentSession"],
		source.join("session.jsonl").to_string_lossy().as_ref()
	);
}

#[test]
fn cross_project_session_id_decline_aborts_with_exit_zero() {
	let (fixture, _source) = fork_fixture("fork-no");
	let (code, stdout, stderr) = fixture.run(
		&fixture.other_project,
		"n\n",
		&["--mode", "rpc", "--session", "cross-1"],
	);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(stdout.contains("Aborted."), "{stdout}");
	assert_eq!(fixture.session_dirs().len(), 1, "no fork on decline");
}

#[test]
fn cross_project_prompt_eof_declines() {
	let (fixture, _source) = fork_fixture("fork-eof");
	let (code, stdout, _stderr) = fixture.run_eof(
		&fixture.other_project,
		&["--mode", "rpc", "--session", "cross-1"],
	);
	assert_eq!(code, Some(0));
	assert!(stdout.contains("Aborted."), "{stdout}");
	assert_eq!(fixture.session_dirs().len(), 1, "no fork on EOF");
}

#[test]
fn cross_project_valued_resume_prompts_the_same_way() {
	let (fixture, _source) = fork_fixture("fork-resume");
	// `--resume <id>` keeps its direct-resume extension for local hits but
	// follows upstream for global hits: the fork prompt applies.
	let (code, stdout, _stderr) = fixture.run(
		&fixture.other_project,
		"y\n",
		&["--mode", "rpc", "--resume", "cross-1"],
	);
	assert_eq!(code, Some(0));
	assert!(
		stdout.contains("Session found in different project:"),
		"{stdout}"
	);
	assert_eq!(fixture.session_dirs().len(), 2, "y forked the session");
}

#[test]
fn cross_project_path_resume_still_opens_directly() {
	let (fixture, source) = fork_fixture("fork-path");
	// The path form never prompts (upstream path branch opens directly); the
	// session keeps its original project cwd (header untouched).
	let file = source.join("session.jsonl").to_string_lossy().into_owned();
	let (code, frame, stderr) =
		fixture.rpc_state(&fixture.other_project, "", &["--session", &file]);
	assert_eq!(code, Some(0), "{stderr}");
	assert!(
		!frame.to_string().contains("different project"),
		"no fork prompt on a path resume"
	);
	assert_eq!(
		frame["data"]["sessionFile"], file,
		"path resume opens the source session itself"
	);
	assert_eq!(
		header_of(&source)["cwd"],
		fixture.project.to_string_lossy().as_ref(),
		"source header keeps the original project cwd"
	);
	assert_eq!(fixture.session_dirs().len(), 1, "no fork created");
}

#[test]
fn local_session_id_still_opens_without_prompt() {
	let (fixture, source) = fork_fixture("fork-local");
	// An exact project hit opens directly — no prompt, no fork.
	let (code, frame, stderr) = fixture.rpc_state(&fixture.project, "", &["--session", "cross-1"]);
	assert_eq!(code, Some(0), "{stderr}");
	assert_eq!(
		frame["data"]["sessionFile"],
		source.join("session.jsonl").to_string_lossy().as_ref()
	);
	assert_eq!(fixture.session_dirs().len(), 1);
}
