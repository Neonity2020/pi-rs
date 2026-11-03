//! Skills + prompt templates end-to-end through the real binary: expansions
//! reach the first user message in json mode, `--no-skills` /
//! `--no-prompt-templates` skip the default directories while explicit
//! `--skill` / `--prompt-template` paths still load, and bad frontmatter
//! degrades to a stderr warning instead of failing startup.

use std::process::{Command, Stdio};

/// The text of the first `message_start` user message from a json-mode run
/// (raw string content, not the JSON-serialized frame).
fn first_user_message_text(output: &[u8]) -> String {
	for line in output.split(|b| *b == b'\n') {
		if line.is_empty() {
			continue;
		}
		let frame: serde_json::Value = serde_json::from_slice(line).unwrap_or_else(|error| {
			panic!("json frame {error} in {}", String::from_utf8_lossy(line))
		});
		if frame["type"] == "message_start" && frame["message"]["role"] == "user" {
			let content = &frame["message"]["content"];
			// Text blocks (`[{"type":"text","text":…}]`) or a bare string.
			if let Some(blocks) = content.as_array() {
				return blocks
					.iter()
					.filter_map(|block| block["text"].as_str())
					.collect::<Vec<_>>()
					.join("");
			}
			if let Some(text) = content.as_str() {
				return text.to_string();
			}
			return content.to_string();
		}
	}
	panic!(
		"no user message_start frame in:\n{}",
		String::from_utf8_lossy(output)
	)
}

struct Fixture {
	root: std::path::PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let root = std::env::temp_dir().join(format!(
			"pi-skills-cli-{}-{}",
			std::process::id(),
			uuid::Uuid::new_v4().simple()
		));
		let project = root.join("project");
		let agent_dir = root.join("agent");
		// User skills dir: one good skill (SKILL.md form) and one with a
		// name-less plain form; the broken one exercises the warning path.
		std::fs::create_dir_all(agent_dir.join("skills/demo")).unwrap();
		std::fs::write(
			agent_dir.join("skills/demo/SKILL.md"),
			"---\nname: demo\ndescription: A demo SKILL.md\n---\nSKILL_BODY_LINE\nStep two: see notes.md\n",
		)
		.unwrap();
		std::fs::create_dir_all(agent_dir.join("skills/broken")).unwrap();
		std::fs::write(
			agent_dir.join("skills/broken/SKILL.md"),
			"---\ndescription: [unclosed\n---\nbody",
		)
		.unwrap();
		// Project prompts dir: one template with frontmatter.
		std::fs::create_dir_all(project.join(".pi/prompts")).unwrap();
		std::fs::write(
			project.join(".pi/prompts/rev.md"),
			"---\ndescription: Review a file\nargument-hint: <path>\n---\nTEMPLATE_BODY first=$1 all=$@\n",
		)
		.unwrap();
		std::fs::create_dir_all(&project).unwrap();
		Self { root }
	}

	fn project(&self) -> std::path::PathBuf {
		self.root.join("project")
	}

	fn agent_dir(&self) -> std::path::PathBuf {
		self.root.join("agent")
	}

	/// Run pi-rs in json mode with the mock-echo model and collect
	/// (stdout, stderr). A leading `/` argument is the prompt.
	fn run(&self, extra_args: &[&str]) -> (Vec<u8>, Vec<u8>, std::process::Output) {
		let child = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(self.project())
			.env("PI_RS_AGENT_DIR", self.agent_dir())
			.env_remove("PI_RS_SESSION_DIR")
			.args([
				"--no-mcp",
				"--mode",
				"json",
				"--no-session",
				"--model",
				"mock/mock-echo",
			])
			.args(extra_args)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		let output = child.wait_with_output().unwrap();
		assert!(
			output.status.success(),
			"exit {:?}\nstderr:\n{}",
			output.status.code(),
			String::from_utf8_lossy(&output.stderr)
		);
		(output.stdout.clone(), output.stderr.clone(), output)
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.root);
	}
}

#[test]
fn template_command_expands_into_the_first_user_message() {
	let fixture = Fixture::new();
	let (stdout, _stderr, _) = fixture.run(&["/rev hello world"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("TEMPLATE_BODY first=hello all=hello world"),
		"expanded text missing from first user message: {user_text}"
	);
	assert!(
		!user_text.contains("/rev"),
		"the raw command must not reach the model: {user_text}"
	);

	// A template name with no arguments expands with empty placeholders.
	let (stdout, _stderr, _) = fixture.run(&["/rev"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("TEMPLATE_BODY first= all="),
		"bare expansion wrong: {user_text}"
	);

	// An unknown template name passes through as an ordinary message
	// (upstream `expandPromptTemplate` returns the original text).
	let (stdout, _stderr, _) = fixture.run(&["/nosuchtemplate arg"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("/nosuchtemplate arg"),
		"unknown template must pass through: {user_text}"
	);
}

#[test]
fn skill_command_expands_with_location_and_args() {
	let fixture = Fixture::new();
	let skill_file = fixture.agent_dir().join("skills/demo/SKILL.md");
	let (stdout, _stderr, _) = fixture.run(&["/skill:demo keep this tail"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains(&format!(
			"<skill name=\"demo\" location=\"{}\">",
			skill_file.display()
		)),
		"skill header wrong: {user_text}"
	);
	assert!(
		user_text.contains(&format!(
			"References are relative to {}.",
			fixture.agent_dir().join("skills/demo").display()
		)),
		"references line wrong: {user_text}"
	);
	assert!(
		user_text.contains("SKILL_BODY_LINE"),
		"body missing: {user_text}"
	);
	assert!(
		!user_text.contains("name: demo"),
		"frontmatter must be stripped: {user_text}"
	);
	assert!(
		user_text.contains("keep this tail"),
		"args must be appended: {user_text}"
	);

	// Unknown skill: pass-through, like upstream `_expandSkillCommand`.
	let (stdout, _stderr, _) = fixture.run(&["/skill:missing nothing"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("/skill:missing nothing"),
		"unknown skill must pass through: {user_text}"
	);
}

#[test]
fn no_skills_skips_defaults_but_explicit_paths_still_load() {
	let fixture = Fixture::new();
	// --no-skills: the default skills dir is skipped, so /skill:demo is
	// unknown and passes through. The `-ns` short form behaves the same.
	for flag in ["--no-skills", "-ns"] {
		let (stdout, _stderr, _) = fixture.run(&[flag, "/skill:demo"]);
		let user_text = first_user_message_text(&stdout);
		assert!(
			user_text.contains("/skill:demo"),
			"{flag} must skip the default dir: {user_text}"
		);
	}

	// The same run with an explicit --skill path still loads it.
	let skill_file = fixture.agent_dir().join("skills/demo/SKILL.md");
	let (stdout, _stderr, _) = fixture.run(&[
		"--no-skills",
		"--skill",
		skill_file.to_str().unwrap(),
		"/skill:demo",
	]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("<skill name=\"demo\""),
		"explicit --skill must still load with --no-skills: {user_text}"
	);
}

#[test]
fn no_prompt_templates_skips_defaults_but_explicit_paths_still_load() {
	let fixture = Fixture::new();
	let (stdout, _stderr, _) = fixture.run(&["--no-prompt-templates", "/rev x"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("/rev x"),
		"--no-prompt-templates must skip the default dir: {user_text}"
	);

	let template = fixture.project().join(".pi/prompts/rev.md");
	let (stdout, _stderr, _) = fixture.run(&[
		"--no-prompt-templates",
		"--prompt-template",
		template.to_str().unwrap(),
		"/rev x",
	]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("TEMPLATE_BODY first=x all=x"),
		"explicit --prompt-template must still load: {user_text}"
	);
}

#[test]
fn broken_frontmatter_warns_and_does_not_crash() {
	let fixture = Fixture::new();
	let (stdout, stderr, _) = fixture.run(&["/skill:broken"]);
	// The broken skill never loaded; the command passes through.
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("/skill:broken"),
		"broken skill must not load: {user_text}"
	);
	// The parse failure surfaces as a warning on stderr; the run succeeds
	// (exit status asserted in `Fixture::run`).
	let stderr_text = String::from_utf8_lossy(&stderr);
	assert!(
		stderr_text.contains("warning: skill:"),
		"expected a skill warning on stderr, got: {stderr_text}"
	);
}

#[test]
fn good_and_broken_skills_coexist_and_the_good_one_expands() {
	let fixture = Fixture::new();
	let (stdout, _stderr, _) = fixture.run(&["/skill:demo"]);
	let user_text = first_user_message_text(&stdout);
	assert!(
		user_text.contains("SKILL_BODY_LINE"),
		"good skill must still expand alongside broken ones: {user_text}"
	);
}
