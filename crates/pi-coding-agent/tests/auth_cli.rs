//! Integration tests for the `pi-rs auth` subcommand (upstream
//! `cli/auth-command.ts` port, cut to models.json API keys) — real-subprocess
//! style like `cli_flags.rs`, temp dirs only, no network and no real keys.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture {
	root: PathBuf,
	project: PathBuf,
	agent: PathBuf,
}

impl Fixture {
	fn new(tag: &str) -> Self {
		let root = std::env::temp_dir().join(format!("pi-auth-cli-{tag}-{}", uuid::Uuid::new_v4()));
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

	/// models.json with three providers: a literal key, an `$ENV` key, and a
	/// provider without any key.
	fn write_models_json(&self) {
		std::fs::write(
			self.agent.join("models.json"),
			r#"{"providers":{
				"acme":{"api":"openai-completions","baseUrl":"https://api.acme.example/v1","apiKey":"sk-literal-acme","models":[{"id":"acme-large","name":"ACME Large"}]},
				"envy":{"api":"openai-completions","baseUrl":"https://envy.example/v1","apiKey":"$ACME_ENV_TOKEN","models":[{"id":"envy-1"}]},
				"bare":{"api":"openai-completions","baseUrl":"https://bare.example/v1","models":[{"id":"bare-1"}]}
			}}"#,
		)
		.unwrap();
	}

	fn run(&self, envs: &[(&str, &str)], args: &[&str]) -> Output {
		Command::new(env!("CARGO_BIN_EXE_pi-rs"))
			.current_dir(&self.project)
			.env("PI_RS_AGENT_DIR", &self.agent)
			.env_remove("PI_RS_SESSION_DIR")
			.env_remove("PI_SESSION_DIR")
			.env_remove("PI_OFFLINE")
			.envs(envs.iter().copied())
			.args(args)
			.stdin(Stdio::null())
			.output()
			.unwrap()
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.root);
	}
}

fn code(output: &Output) -> i32 {
	output.status.code().unwrap_or(-1)
}

fn stdout(output: &Output) -> String {
	String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

// -- help / usage ---------------------------------------------------------------

#[test]
fn auth_help_prints_the_upstream_usage_lines() {
	let fixture = Fixture::new("help");
	for args in [
		vec!["auth"],
		vec!["auth", "help"],
		vec!["auth", "--help"],
		vec!["auth", "-h"],
		vec!["auth", "check", "--help"],
	] {
		let output = fixture.run(&[], &args);
		assert_eq!(code(&output), 0, "args {:?}: {}", args, stderr(&output));
		let out = stdout(&output);
		assert!(
			out.contains("  pi-rs auth print-api-key [--provider <provider>] [--model <model>]"),
			"{out}"
		);
		assert!(
			out.contains("  pi-rs auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]"),
			"{out}"
		);
		assert!(
			out.contains("  pi-rs auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]"),
			"{out}"
		);
		assert!(
			out.contains("Auth commands require at least one of --provider or --model."),
			"{out}"
		);
	}
}

#[test]
fn unknown_auth_command_errors_with_the_upstream_text() {
	let fixture = Fixture::new("unknown");
	let output = fixture.run(&[], &["auth", "bogus"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains(
			"Unknown auth command \"bogus\". Use \"pi-rs auth print-api-key\", \"pi-rs auth print-bearer-token\", or \"pi-rs auth check\"."
		),
		"{}",
		stderr(&output)
	);
}

// -- auth check -----------------------------------------------------------------

#[test]
fn auth_check_ready_with_literal_key() {
	let fixture = Fixture::new("check-ready");
	fixture.write_models_json();
	let output = fixture.run(&[], &["auth", "check", "--provider", "acme"]);
	assert_eq!(code(&output), 0, "{}", stderr(&output));
	assert_eq!(stdout(&output).trim(), "ready");
	// --json shape: status/provider/authType (+ credentials with
	// --credentials).
	let output = fixture.run(&[], &["auth", "check", "--provider", "acme", "--json"]);
	assert_eq!(code(&output), 0);
	let value: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
	assert_eq!(value["status"], "ready");
	assert_eq!(value["provider"], "acme");
	assert_eq!(value["authType"], "api_key");
	assert!(value.get("credentials").is_none());
	let output = fixture.run(
		&[],
		&[
			"auth",
			"check",
			"--provider",
			"acme",
			"--json",
			"--credentials",
		],
	);
	assert_eq!(code(&output), 0);
	let value: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
	assert_eq!(value["credentials"], "sk-literal-acme");
}

#[test]
fn auth_check_resolves_provider_from_model() {
	let fixture = Fixture::new("check-model");
	fixture.write_models_json();
	let output = fixture.run(&[], &["auth", "check", "--model", "acme-large"]);
	assert_eq!(code(&output), 0, "{}", stderr(&output));
	assert_eq!(stdout(&output).trim(), "ready");
	let output = fixture.run(&[], &["auth", "check", "--model", "acme-large", "--json"]);
	let value: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
	assert_eq!(value["provider"], "acme");
	// An unknown model is an AuthCommandError: exit 2 for check.
	let output = fixture.run(&[], &["auth", "check", "--model", "no-such-model"]);
	assert_eq!(code(&output), 2);
	assert!(
		stderr(&output).contains("Unable to resolve model \"no-such-model\""),
		"{}",
		stderr(&output)
	);
}

#[test]
fn auth_check_env_key_resolves() {
	let fixture = Fixture::new("check-env");
	fixture.write_models_json();
	let output = fixture.run(
		&[("ACME_ENV_TOKEN", "tok-env-123")],
		&["auth", "check", "--provider", "envy"],
	);
	assert_eq!(code(&output), 0, "{}", stderr(&output));
	assert_eq!(stdout(&output).trim(), "ready");
	// Without the env var the template is unresolved → not_ready.
	let output = fixture.run(&[], &["auth", "check", "--provider", "envy"]);
	assert_eq!(code(&output), 1);
	assert_eq!(stdout(&output).trim(), "not_ready");
}

#[test]
fn auth_check_not_ready_states_and_exit_codes() {
	let fixture = Fixture::new("check-not-ready");
	fixture.write_models_json();
	// Provider without any key: credentials_not_configured, exit 1.
	let output = fixture.run(&[], &["auth", "check", "--provider", "bare"]);
	assert_eq!(code(&output), 1);
	assert_eq!(stdout(&output).trim(), "not_ready");
	// Unknown provider: provider_not_found, exit 1.
	let output = fixture.run(&[], &["auth", "check", "--provider", "ghost", "--json"]);
	assert_eq!(code(&output), 1);
	let value: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
	assert_eq!(value["status"], "not_ready");
	assert_eq!(value["reason"], "provider_not_found");
	assert_eq!(value["provider"], "ghost");
	// Missing --provider/--model is an AuthCommandError: exit 2.
	let output = fixture.run(&[], &["auth", "check"]);
	assert_eq!(code(&output), 2);
	assert!(
		stderr(&output).contains("Auth checks require --provider <provider> or --model <model>"),
		"{}",
		stderr(&output)
	);
}

// -- auth print-api-key ---------------------------------------------------------

#[test]
fn print_api_key_prints_the_resolved_key() {
	let fixture = Fixture::new("key-print");
	fixture.write_models_json();
	let output = fixture.run(&[], &["auth", "print-api-key", "--provider", "acme"]);
	assert_eq!(code(&output), 0, "{}", stderr(&output));
	assert_eq!(stdout(&output), "sk-literal-acme\n");
	// --model form resolves through the provider scan.
	let output = fixture.run(&[], &["auth", "print-api-key", "--model", "acme-large"]);
	assert_eq!(code(&output), 0);
	assert_eq!(stdout(&output), "sk-literal-acme\n");
	// $ENV form resolves from the process environment.
	let output = fixture.run(
		&[("ACME_ENV_TOKEN", "tok-env-123")],
		&["auth", "print-api-key", "--provider", "envy"],
	);
	assert_eq!(code(&output), 0);
	assert_eq!(stdout(&output), "tok-env-123\n");
}

#[test]
fn print_api_key_errors_carry_upstream_texts() {
	let fixture = Fixture::new("key-errors");
	fixture.write_models_json();
	// Key missing/unresolvable.
	let output = fixture.run(&[], &["auth", "print-api-key", "--provider", "bare"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("No usable API key is configured"),
		"{}",
		stderr(&output)
	);
	// Unknown provider.
	let output = fixture.run(&[], &["auth", "print-api-key", "--provider", "ghost"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output)
			.contains("Unknown provider \"ghost\". Use --list-models to see available providers."),
		"{}",
		stderr(&output)
	);
	// Missing --provider/--model: exit 1 for print commands.
	let output = fixture.run(&[], &["auth", "print-api-key"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output)
			.contains("Credential printing requires --provider <provider> or --model <model>"),
		"{}",
		stderr(&output)
	);
}

// -- auth print-bearer-token ----------------------------------------------------

#[test]
fn print_bearer_token_reports_no_oauth_credentials() {
	let fixture = Fixture::new("bearer");
	fixture.write_models_json();
	let output = fixture.run(&[], &["auth", "print-bearer-token", "--provider", "acme"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("Provider \"acme\" is not configured with an OAuth bearer token"),
		"{}",
		stderr(&output)
	);
	// Unknown provider is reported first, like upstream.
	let output = fixture.run(&[], &["auth", "print-bearer-token", "--provider", "ghost"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("Unknown provider \"ghost\""),
		"{}",
		stderr(&output)
	);
	// --min-expiry parses (declared no-op) instead of erroring.
	let output = fixture.run(
		&[],
		&[
			"auth",
			"print-bearer-token",
			"--provider",
			"acme",
			"--min-expiry",
			"30m",
		],
	);
	assert_eq!(code(&output), 1);
	assert!(
		!stderr(&output).contains("--min-expiry"),
		"{}",
		stderr(&output)
	);
	// A malformed duration errors with the upstream text.
	let output = fixture.run(
		&[],
		&[
			"auth",
			"print-bearer-token",
			"--provider",
			"acme",
			"--min-expiry",
			"soon",
		],
	);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("--min-expiry must use a duration such as 30m or 1h"),
		"{}",
		stderr(&output)
	);
}

// -- flag ownership / validation ------------------------------------------------

#[test]
fn auth_flag_restrictions_follow_upstream() {
	let fixture = Fixture::new("flags");
	fixture.write_models_json();
	// --min-expiry belongs to print-bearer-token only.
	let output = fixture.run(
		&[],
		&["auth", "check", "--provider", "acme", "--min-expiry", "30m"],
	);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("--min-expiry is only supported by print-bearer-token"),
		"{}",
		stderr(&output)
	);
	// --json/--credentials/--no-refresh belong to auth check only.
	let output = fixture.run(
		&[],
		&["auth", "print-api-key", "--provider", "acme", "--json"],
	);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("--json is only supported by auth check"),
		"{}",
		stderr(&output)
	);
	// --no-refresh parses on check (declared no-op) and still succeeds.
	let output = fixture.run(
		&[],
		&["auth", "check", "--provider", "acme", "--no-refresh"],
	);
	assert_eq!(code(&output), 0, "{}", stderr(&output));
	// Unknown option reports with the usage hint, exit 1.
	let output = fixture.run(&[], &["auth", "check", "--provider", "acme", "--bogus"]);
	assert_eq!(code(&output), 1);
	assert!(
		stderr(&output).contains("Unknown option --bogus for \"auth check\"."),
		"{}",
		stderr(&output)
	);
	assert!(
		stderr(&output).contains("Use \"pi-rs --help\" or"),
		"{}",
		stderr(&output)
	);
	// Positionals and --api-key are rejected with the upstream text (exit 2
	// for check).
	let output = fixture.run(&[], &["auth", "check", "--provider", "acme", "extra"]);
	assert_eq!(code(&output), 2);
	assert!(
		stderr(&output).contains("Auth commands only accept --provider and --model"),
		"{}",
		stderr(&output)
	);
	let output = fixture.run(
		&[],
		&["auth", "check", "--provider", "acme", "--api-key", "sk"],
	);
	assert_eq!(code(&output), 2);
	assert!(
		stderr(&output).contains("Auth commands only accept --provider and --model"),
		"{}",
		stderr(&output)
	);
}

#[test]
fn main_cli_flags_still_parse_after_auth_split() {
	let fixture = Fixture::new("main-cli");
	fixture.write_models_json();
	// `--version` and `-h` are unaffected by the auth interception.
	let output = fixture.run(&[], &["--version"]);
	assert_eq!(code(&output), 0);
	assert_eq!(
		stdout(&output).trim(),
		format!("pi-rs {}", env!("CARGO_PKG_VERSION"))
	);
	let output = fixture.run(&[], &["-h"]);
	assert_eq!(code(&output), 0);
	assert!(stdout(&output).contains("Usage:"));
	// The word "auth" as a prompt positional still reaches clap (only a
	// leading `auth` subcommand short-circuits).
	let output = fixture.run(
		&[],
		&[
			"--no-mcp",
			"--mode",
			"json",
			"--no-session",
			"--model",
			"mock/mock-echo",
			"auth",
		],
	);
	assert!(output.status.success(), "{}", stderr(&output));
}
