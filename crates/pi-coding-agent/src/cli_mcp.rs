//! `pi mcp` — add, remove, and check MCP servers and sign in to them outside
//! a session — port of
//! `pi/packages/coding-agent/src/extensions/mcp/cli.ts`. Agents run it
//! through bash to configure servers, verify an `mcp.json` they wrote, and
//! start an OAuth sign-in; the user only approves access in the browser.
//! Running sessions pick up new credentials on their next turn.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::core::mcp_config::{
	self, McpConfig, McpServerEntry, add_mcp_server_config, mcp_json_path,
	remove_mcp_server_config, validate_server_entry,
};
use crate::core::open_browser::open_browser;
use crate::core::tools::{McpProbeState, probe_mcp_entry};

const APP_NAME: &str = "pi-rs";
const HELP: &str = "\
Usage:
  pi-rs mcp add <server> [options] -- <command> [args...]
  pi-rs mcp add <server> [options] --url <url>
  pi-rs mcp remove <server> [-l]
  pi-rs mcp list [--json]
  pi-rs mcp login <server> [--timeout <seconds>]
  pi-rs mcp logout <server>

Configure and check MCP servers and sign in to OAuth servers without starting a session.
Reads ~/.pi-rs/agent/mcp.json and, in trusted projects, .pi/mcp.json.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add and remove:
  -l, --local             Use .pi/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send \"Authorization: Bearer ${NAME}\"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${NAME} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --exposure <mode>       codemode (default), deferred, direct, or hidden
  --description <text>    What the server offers, shown in the system prompt

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     Time limit for the whole sign-in (default: 300)";

const HELP_HINT: &str = "Use \"pi-rs mcp --help\" for usage.";
const DEFAULT_LOGIN_TIMEOUT_SECONDS: u64 = 300;

/// Short spellings of options. `-l`/`--local` match `pi install`.
const OPTION_ALIASES: &[(&str, &str)] = &[("-l", "--local")];

/// One parsed command line — upstream `ParsedOptions`.
struct ParsedOptions {
	positional: Vec<String>,
	/// `--name value` options.
	values: std::collections::HashMap<String, String>,
	/// Bare `--flag` options.
	flags: std::collections::HashSet<String>,
	/// Values of repeatable `--name value` options, in order.
	lists: std::collections::HashMap<String, Vec<String>>,
}

/// Parse `--name value` options; returns `None` and reports unknown ones.
/// Kinds: `None` (flag), `Some("value")`, `Some("list")`. `--` ends the
/// options, as does reaching `max_positionals` positional arguments: the
/// remaining arguments are positional, so a command's own options
/// (`add <server> <command> --flag`) are passed through — upstream
/// `parseOptions`.
fn parse_options(
	args: &[String],
	known: &[(&str, Option<&str>)],
	error: &mut dyn FnMut(&str),
	max_positionals: usize,
) -> Option<ParsedOptions> {
	let mut parsed = ParsedOptions {
		positional: Vec::new(),
		values: Default::default(),
		flags: Default::default(),
		lists: Default::default(),
	};
	let mut index = 0;
	while index < args.len() {
		let raw = &args[index];
		let arg = OPTION_ALIASES
			.iter()
			.find(|(short, _)| short == raw)
			.map(|(_, long)| (*long).to_string())
			.unwrap_or_else(|| raw.clone());
		if arg == "--" || parsed.positional.len() >= max_positionals {
			let rest = if arg == "--" { index + 1 } else { index };
			parsed.positional.extend(args[rest..].iter().cloned());
			break;
		}
		if !arg.starts_with("--") {
			parsed.positional.push(arg);
			index += 1;
			continue;
		}
		let name = arg.trim_start_matches('-').to_string();
		let Some(kind) = known
			.iter()
			.find(|(known, _)| *known == name.as_str())
			.map(|(_, kind)| *kind)
		else {
			error(&format!("Unknown option {arg}.\n{HELP_HINT}"));
			return None;
		};
		match kind {
			None => {
				parsed.flags.insert(name);
			}
			Some("value") | Some("list") => {
				index += 1;
				let Some(value) = args.get(index) else {
					error(&format!("{arg} needs a value."));
					return None;
				};
				if kind == Some("list") {
					parsed.lists.entry(name).or_default().push(value.clone());
				} else {
					parsed.values.insert(name, value.clone());
				}
			}
			Some(other) => unreachable!("unknown option kind {other:?}"),
		}
		index += 1;
	}
	Some(parsed)
}

/// Run `pi mcp <args>` and return the exit code (upstream `runMcpCommand`).
pub async fn run_mcp_command(args: &[String]) -> i32 {
	let log = |line: &str| println!("{line}");
	let mut error = |line: &str| eprintln!("{line}");
	let Some(command) = args.first() else {
		log(HELP);
		return 0;
	};
	if command == "help" || args.iter().any(|arg| arg == "--help" || arg == "-h") {
		log(HELP);
		return 0;
	}
	let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
	let project_config = cwd.join(".pi").join("mcp.json");
	if command == "add" || command == "remove" {
		return if command == "add" {
			add(&args[1..], &project_config, &log, &mut error)
		} else {
			remove(&args[1..], &cwd, &project_config, &log, &mut error).await
		};
	}
	let project_trusted = crate::core::trust::is_trusted(&cwd);
	let loaded = mcp_config::load_mcp_config_paths(&mcp_json_path(), &cwd, project_trusted).await;
	let untrusted_note = if !project_trusted && project_config.exists() {
		Some(format!(
			"{} is ignored because the project is not trusted. Start {APP_NAME} in the project to trust it.",
			project_config.display()
		))
	} else {
		None
	};

	match command.as_str() {
		"list" => {
			let Some(parsed) = parse_options(&args[1..], &[("json", None)], &mut error, usize::MAX)
			else {
				return 1;
			};
			if !parsed.positional.is_empty() {
				error(&format!("Usage: {APP_NAME} mcp list [--json]\n{HELP_HINT}"));
				return 1;
			}
			list(
				&loaded,
				parsed.flags.contains("json"),
				untrusted_note.as_deref(),
				&log,
			)
			.await
		}
		"login" | "logout" => {
			let known: &[(&str, Option<&str>)] = if command == "login" {
				&[("timeout", Some("value"))]
			} else {
				&[]
			};
			let Some(parsed) = parse_options(&args[1..], known, &mut error, usize::MAX) else {
				return 1;
			};
			let [name, extra @ ..] = parsed.positional.as_slice() else {
				error(&format!(
					"Usage: {APP_NAME} mcp {command} <server>\n{HELP_HINT}"
				));
				return 1;
			};
			if !extra.is_empty() {
				error(&format!(
					"Usage: {APP_NAME} mcp {command} <server>\n{HELP_HINT}"
				));
				return 1;
			}
			let Some(entry) = loaded.servers.iter().find(|server| server.name == *name) else {
				let configured = loaded
					.servers
					.iter()
					.map(|server| server.name.as_str())
					.collect::<Vec<_>>()
					.join(", ");
				error(&format!(
					"No MCP server named \"{name}\".{} Configured: {}.",
					untrusted_note
						.as_deref()
						.map(|note| format!(" {note}"))
						.unwrap_or_default(),
					if configured.is_empty() {
						"none"
					} else {
						configured.as_str()
					},
				));
				return 1;
			};
			let Some(url) = oauth_url(entry) else {
				error(&format!(
					"MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
				));
				return 1;
			};
			if command == "logout" {
				match logout(entry, &url).await {
					Ok(true) => {
						log(&format!("Signed out of MCP server \"{name}\"."));
						0
					}
					Ok(false) => {
						log(&format!("No stored credentials for MCP server \"{name}\"."));
						0
					}
					Err(message) => {
						error(&message);
						1
					}
				}
			} else {
				let timeout = match parsed
					.values
					.get("timeout")
					.map(|value| value.parse::<u64>())
				{
					None => DEFAULT_LOGIN_TIMEOUT_SECONDS,
					Some(Ok(timeout)) if timeout > 0 => timeout,
					Some(_) => {
						error("--timeout must be a positive number of seconds.");
						return 1;
					}
				};
				login(entry, &url, Duration::from_secs(timeout), &log, &mut error).await
			}
		}
		other => {
			error(&format!("Unknown mcp command \"{other}\".\n{HELP_HINT}"));
			1
		}
	}
}

/// Whether an HTTP server authenticates with OAuth: a `url` without an
/// `Authorization` header (upstream `usesOAuth`).
fn oauth_url(entry: &McpServerEntry) -> Option<String> {
	let url = entry.url.as_ref()?;
	let has_authorization = entry
		.headers
		.iter()
		.any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
	if has_authorization {
		None
	} else {
		Some(url.clone())
	}
}

/// Parse `KEY=VALUE` pairs of a repeatable option — upstream `parsePairs`.
fn parse_pairs(
	option: &str,
	pairs: Option<&Vec<String>>,
	error: &mut dyn FnMut(&str),
) -> Option<Vec<(String, String)>> {
	let Some(pairs) = pairs else {
		return Some(Vec::new());
	};
	let mut record = Vec::new();
	for pair in pairs {
		let Some(separator) = pair.find('=') else {
			error(&format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
			return None;
		};
		if separator == 0 {
			error(&format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
			return None;
		}
		record.push((
			pair[..separator].to_string(),
			pair[separator + 1..].to_string(),
		));
	}
	Some(record)
}

/// `pi mcp add` — assemble the config, validate, write (upstream `add`).
fn add(
	args: &[String],
	project_config: &Path,
	log: &dyn Fn(&str),
	error: &mut dyn FnMut(&str),
) -> i32 {
	let usage = format!(
		"Usage: {APP_NAME} mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}"
	);
	// Two positionals (`<server> <command>` for stdio) end the options; the
	// rest are the server command's own arguments.
	let Some(parsed) = parse_options(
		args,
		&[
			("local", None),
			("url", Some("value")),
			("env", Some("list")),
			("cwd", Some("value")),
			("header", Some("list")),
			("bearer-token-env-var", Some("value")),
			("oauth-client-id", Some("value")),
			("oauth-client-secret", Some("value")),
			("oauth-callback-port", Some("value")),
			("oauth-client-name", Some("value")),
			("exposure", Some("value")),
			("description", Some("value")),
		],
		error,
		2,
	) else {
		return 1;
	};
	let [name, command @ ..] = parsed.positional.as_slice() else {
		error(&usage);
		return 1;
	};
	let url = parsed.values.get("url");
	let has_url = url.is_some();
	let has_command = !command.is_empty();
	if name.is_empty() || has_url == has_command {
		error(&usage);
		return 1;
	}
	let value = |option: &str| parsed.values.get(option).cloned();
	let exposure = value("exposure");
	let http_only = [
		"header",
		"bearer-token-env-var",
		"oauth-client-id",
		"oauth-client-secret",
		"oauth-callback-port",
		"oauth-client-name",
	];
	let stdio_only = ["env", "cwd"];
	let misplaced_scope: &[&str] = if has_url { &stdio_only } else { &http_only };
	if let Some(misplaced) = misplaced_scope
		.iter()
		.find(|option| parsed.values.contains_key(**option) || parsed.lists.contains_key(**option))
	{
		let applies_to = if has_url {
			"stdio servers"
		} else {
			"HTTP servers (--url)"
		};
		error(&format!("--{misplaced} only applies to {applies_to}."));
		return 1;
	}

	let mut config = serde_json::Map::new();
	if let Some(url) = url {
		let Some(headers) = parse_pairs("header", parsed.lists.get("header"), error) else {
			return 1;
		};
		let mut header_map = serde_json::Map::new();
		for (key, header_value) in headers {
			header_map.insert(key, Value::String(header_value));
		}
		if let Some(bearer) = value("bearer-token-env-var") {
			header_map.insert(
				"Authorization".to_string(),
				Value::String(format!("Bearer ${{{bearer}}}")),
			);
		}
		let mut oauth = serde_json::Map::new();
		if let Some(client_id) = value("oauth-client-id") {
			oauth.insert("clientId".to_string(), Value::String(client_id));
		}
		if let Some(secret) = value("oauth-client-secret") {
			oauth.insert("clientSecret".to_string(), Value::String(secret));
		}
		if let Some(port) = value("oauth-callback-port") {
			match port.parse::<u64>() {
				Ok(port) if (1..=65535).contains(&port) => {
					oauth.insert("callbackPort".to_string(), json!(port));
				}
				_ => {
					error("--oauth-callback-port must be a port number.");
					return 1;
				}
			}
		}
		if let Some(client_name) = value("oauth-client-name") {
			oauth.insert("clientName".to_string(), Value::String(client_name));
		}
		config.insert("url".to_string(), Value::String(url.clone()));
		if !header_map.is_empty() {
			config.insert("headers".to_string(), Value::Object(header_map));
		}
		if !oauth.is_empty() {
			config.insert("oauth".to_string(), Value::Object(oauth));
		}
	} else {
		let Some(env) = parse_pairs("env", parsed.lists.get("env"), error) else {
			return 1;
		};
		let mut env_map = serde_json::Map::new();
		for (key, env_value) in env {
			env_map.insert(key, Value::String(env_value));
		}
		config.insert("command".to_string(), Value::String(command[0].clone()));
		if command.len() > 1 {
			config.insert(
				"args".to_string(),
				Value::Array(
					command[1..]
						.iter()
						.map(|arg| Value::String(arg.clone()))
						.collect(),
				),
			);
		}
		if !env_map.is_empty() {
			config.insert("env".to_string(), Value::Object(env_map));
		}
		if let Some(cwd) = value("cwd") {
			config.insert("cwd".to_string(), Value::String(cwd));
		}
	}
	if let Some(exposure) = exposure {
		config.insert("exposure".to_string(), Value::String(exposure));
	}
	if let Some(description) = value("description") {
		config.insert("description".to_string(), Value::String(description));
	}
	let config = Value::Object(config);
	if let Err(message) = validate_server_entry(name, &config) {
		error(&message);
		return 1;
	}

	let project = parsed.flags.contains("local");
	let path = if project {
		project_config.to_path_buf()
	} else {
		mcp_json_path()
	};
	let scope = if project { "project" } else { "global" };
	let replaced = match add_mcp_server_config(&path, name, config) {
		Ok(replaced) => replaced,
		Err(message) => {
			error(&format!("Could not update {}: {message}", path.display()));
			return 1;
		}
	};
	log(&format!(
		"{} {scope} MCP server \"{name}\" in {}.",
		if replaced { "Replaced" } else { "Added" },
		path.display()
	));
	if project && !crate::core::trust::is_trusted(&std::env::current_dir().unwrap_or_default()) {
		log(&format!(
			"The project is not trusted, so {} is ignored until you start {APP_NAME} in the project and trust it.",
			path.display()
		));
	}
	// HTTP servers without an Authorization header may use OAuth.
	let may_need_sign_in = has_url
		&& !parsed
			.lists
			.get("header")
			.into_iter()
			.flatten()
			.chain(
				parsed
					.values
					.get("bearer-token-env-var")
					.map(|_| "Authorization=Bearer *".to_string())
					.iter(),
			)
			.any(|pair| {
				pair.split('=')
					.next()
					.is_some_and(|name| name.eq_ignore_ascii_case("authorization"))
			});
	log(&if may_need_sign_in {
		format!(
			"Check it with: {APP_NAME} mcp list. If it requires sign-in: {APP_NAME} mcp login {name}"
		)
	} else {
		format!("Check it with: {APP_NAME} mcp list")
	});
	0
}

/// Whether one `mcp.json` file defines a server `name` — the source check the
/// remove hint needs (upstream keeps `source` on each entry; pi-rs's loader
/// does not, so the two files are read directly).
fn file_defines(path: &Path, name: &str) -> bool {
	std::fs::read_to_string(path)
		.ok()
		.and_then(|text| serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')).ok())
		.and_then(|value| value.get("mcpServers").and_then(Value::as_object).cloned())
		.is_some_and(|servers| servers.contains_key(name))
}

/// `pi mcp remove` — delete one entry (upstream `remove`).
async fn remove(
	args: &[String],
	cwd: &Path,
	project_config: &Path,
	log: &dyn Fn(&str),
	error: &mut dyn FnMut(&str),
) -> i32 {
	let Some(parsed) = parse_options(args, &[("local", None)], error, usize::MAX) else {
		return 1;
	};
	let [name, extra @ ..] = parsed.positional.as_slice() else {
		error(&format!(
			"Usage: {APP_NAME} mcp remove <server> [-l]\n{HELP_HINT}"
		));
		return 1;
	};
	if !extra.is_empty() {
		error(&format!(
			"Usage: {APP_NAME} mcp remove <server> [-l]\n{HELP_HINT}"
		));
		return 1;
	}
	let project = parsed.flags.contains("local");
	let path = if project {
		project_config.to_path_buf()
	} else {
		mcp_json_path()
	};
	let scope = if project { "project" } else { "global" };
	let removed = match remove_mcp_server_config(&path, name) {
		Ok(removed) => removed,
		Err(message) => {
			error(&format!("Could not update {}: {message}", path.display()));
			return 1;
		}
	};
	if removed {
		log(&format!(
			"Removed {scope} MCP server \"{name}\" from {}.",
			path.display()
		));
		return 0;
	}
	// Is it defined in the other scope? (Upstream reports the other entry's
	// source file and the flag to reach it.)
	let other_scope_is_project = !project;
	let other_path = if other_scope_is_project {
		project_config.to_path_buf()
	} else {
		mcp_json_path()
	};
	if file_defines(&other_path, name) {
		let flag = if other_scope_is_project {
			"--local"
		} else {
			"no --local"
		};
		error(&format!(
			"No {scope} MCP server named \"{name}\" in {}. It is defined in {}; use {flag}.",
			path.display(),
			other_path.display()
		));
	} else {
		error(&format!(
			"No {scope} MCP server named \"{name}\" in {}.",
			path.display()
		));
	}
	let _ = cwd;
	1
}

/// `pi mcp list` — per-server state report (upstream `list`).
async fn list(
	loaded: &McpConfig,
	json: bool,
	untrusted_note: Option<&str>,
	log: &dyn Fn(&str),
) -> i32 {
	let mut reports = Vec::new();
	for entry in &loaded.servers {
		let transport = match &entry.url {
			Some(url) => url.clone(),
			None => std::iter::once(entry.command.clone())
				.chain(entry.args.iter().cloned())
				.collect::<Vec<_>>()
				.join(" "),
		};
		if !entry.enabled {
			reports.push(json!({
				"name": entry.name,
				"scope": "global",
				"enabled": false,
				"exposure": "codemode",
				"transport": transport,
				"state": "disabled",
				"tools": [],
			}));
			continue;
		}
		let (state, _challenge) = probe_mcp_entry(entry).await;
		let (state_string, tools, error) = match &state {
			McpProbeState::Connected { tools } => ("connected", tools.clone(), None),
			McpProbeState::NeedsAuth => ("needs-auth", Vec::new(), None),
			McpProbeState::Failed(message) => ("failed", Vec::new(), Some(message.clone())),
		};
		reports.push(json!({
			"name": entry.name,
			"scope": "global",
			"enabled": true,
			"exposure": "codemode",
			"transport": transport,
			"state": state_string,
			"tools": tools,
			"error": error,
		}));
	}
	let failed = !loaded.errors.is_empty()
		|| reports.iter().any(|report| {
			report["enabled"].as_bool() == Some(true) && report["state"] != "connected"
		});

	if json {
		let payload = json!({
			"servers": reports,
			"errors": loaded.errors,
			"note": untrusted_note,
		});
		log(&serde_json::to_string_pretty(&payload).unwrap_or_default());
		return if failed { 1 } else { 0 };
	}
	if reports.is_empty() && loaded.errors.is_empty() {
		log(&format!(
			"No MCP servers configured. Add them to {} or .pi/mcp.json.",
			mcp_json_path().display()
		));
	}
	for report in &reports {
		let name = report["name"].as_str().unwrap_or_default();
		let state = report["state"].as_str().unwrap_or_default();
		let exposure = report["exposure"].as_str().unwrap_or_default();
		let scope = report["scope"].as_str().unwrap_or_default();
		let tool_count = report["tools"].as_array().map(Vec::len).unwrap_or(0);
		let state_line = match state {
			"connected" => format!(
				"connected, {tool_count} tool{}",
				if tool_count == 1 { "" } else { "s" }
			),
			"needs-auth" => "needs sign-in".to_string(),
			other => other.to_string(),
		};
		log(&format!("{name}: {state_line} ({exposure}, {scope})"));
		log(&format!(
			"  {}",
			report["transport"].as_str().unwrap_or_default()
		));
		if state == "needs-auth" {
			log(&format!("  sign in with: {APP_NAME} mcp login {name}"));
		}
		if let Some(tools) = report["tools"].as_array()
			&& !tools.is_empty()
		{
			let tools = tools
				.iter()
				.map(|tool| tool.as_str().unwrap_or_default())
				.collect::<Vec<_>>()
				.join(", ");
			log(&format!("  tools: {tools}"));
		}
		if let Some(error) = report["error"].as_str() {
			log(&format!("  {}", error.replace('\n', "\n  ")));
		}
	}
	for config_error in &loaded.errors {
		log(&format!("config error: {config_error}"));
	}
	if let Some(note) = untrusted_note {
		log(note);
	}
	if failed { 1 } else { 0 }
}

/// Delete one server's stored OAuth credentials (upstream the CLI logout
/// path through `McpOAuthCredentialStore.remove`).
async fn logout(entry: &McpServerEntry, url: &str) -> Result<bool, String> {
	pi_mcp::oauth::FileStateStore::new(crate::config::agent_dir().join("mcp-auth.json"))
		.remove(&entry.name, url)
		.await
}

/// The CLI sign-in prompt (upstream the `prompt` object of cli.ts `login`):
/// print the URL and open the browser; the pasted redirect URL is read from
/// stdin when the terminal is interactive.
struct CliPrompt {
	name: String,
	interactive: bool,
}

#[async_trait::async_trait]
impl pi_mcp::oauth::SignInPrompt for CliPrompt {
	fn show_authorization_url(&self, url: &url::Url) {
		println!(
			"Sign in to MCP server \"{}\" in your browser:\n{url}",
			self.name
		);
		open_browser(url.as_str());
	}

	async fn prompt_for_redirect_url(
		&self,
		cancel: tokio_util::sync::CancellationToken,
	) -> Option<String> {
		if !self.interactive {
			// Only the browser callback can finish; wake on cancellation.
			cancel.cancelled().await;
			return None;
		}
		eprint!("If the browser cannot reach this machine, paste the URL it was redirected to: ");
		tokio::select! {
			_ = cancel.cancelled() => None,
			line = read_stdin_line() => line,
		}
	}
}

/// One stdin line (`None` at EOF) — upstream's readline prompt.
async fn read_stdin_line() -> Option<String> {
	use tokio::io::AsyncBufReadExt as _;
	let mut line = String::new();
	match tokio::io::BufReader::new(tokio::io::stdin())
		.read_line(&mut line)
		.await
	{
		Ok(0) | Err(_) => None,
		Ok(_) => Some(line),
	}
}

/// `pi mcp login` — probe, sign in, reconnect (upstream `login`).
async fn login(
	entry: &McpServerEntry,
	url: &str,
	timeout: Duration,
	log: &dyn Fn(&str),
	error: &mut dyn FnMut(&str),
) -> i32 {
	let name = &entry.name;
	// Connecting first answers whether a sign-in is needed and records the
	// server's challenge.
	let (state, challenge) = probe_mcp_entry(entry).await;
	match state {
		McpProbeState::Connected { tools } => {
			log(&format!(
				"Already signed in to MCP server \"{name}\" ({} tools).",
				tools.len()
			));
			return 0;
		}
		McpProbeState::Failed(message) => {
			error(&format!(
				"MCP server \"{name}\" failed to connect: {message}"
			));
			return 1;
		}
		McpProbeState::NeedsAuth => {}
	}
	let interactive = std::io::stdin().is_terminal();
	let cancel = pi_mcp::oauth::sign_in::cancel_after(timeout);
	let options = pi_mcp::oauth::SignInOptions {
		server_url: url.to_string(),
		store: pi_mcp::oauth::FileStateStore::new(crate::config::agent_dir().join("mcp-auth.json"))
			.server(name, url),
		settings: entry.oauth.clone().unwrap_or_default(),
		challenge,
		cancel,
	};
	let prompt = CliPrompt {
		name: name.clone(),
		interactive,
	};
	if let Err(sign_in_error) = pi_mcp::oauth::sign_in(options, &prompt).await {
		let message = match sign_in_error {
			pi_mcp::oauth::SignInError::Cancelled => format!(
				"Sign-in to MCP server \"{name}\" was cancelled or not completed within {} seconds.",
				timeout.as_secs()
			),
			other => format!("Sign-in to MCP server \"{name}\" failed: {other}"),
		};
		error(&message);
		return 1;
	}
	// Reconnect to answer with the fresh credentials (upstream
	// `connection.reconnect`).
	let (state, _) = probe_mcp_entry(entry).await;
	match state {
		McpProbeState::Connected { tools } => {
			log(&format!(
				"Signed in to MCP server \"{name}\" ({} tools).",
				tools.len()
			));
			0
		}
		McpProbeState::NeedsAuth => {
			error(&format!(
				"Signed in, but MCP server \"{name}\" still requires authentication"
			));
			1
		}
		McpProbeState::Failed(message) => {
			error(&format!("Signed in, but {message}"));
			1
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(list: &[&str]) -> Vec<String> {
		list.iter().map(|arg| (*arg).to_string()).collect()
	}

	/// Isolate the agent dir per test (config::agent_dir reads the env at
	/// call time). Returns the temp agent dir.
	fn with_agent_dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-mcp-cli-{name}-{}-{}",
			std::process::id(),
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap()
				.as_nanos()
		));
		std::fs::create_dir_all(&dir).unwrap();
		unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, &dir) };
		dir
	}

	#[test]
	fn parse_options_follows_the_upstream_ladder() {
		let mut error = |_line: &str| panic!("unexpected error");
		let parsed = parse_options(
			&args(&[
				"-l",
				"--url",
				"https://x.com/mcp",
				"--header",
				"A=1",
				"--header",
				"B=2",
				"--json",
			]),
			&[
				("local", None),
				("url", Some("value")),
				("header", Some("list")),
				("json", None),
			],
			&mut error,
			usize::MAX,
		)
		.unwrap();
		assert!(parsed.flags.contains("local"));
		assert!(parsed.flags.contains("json"));
		assert_eq!(
			parsed.values.get("url").map(String::as_str),
			Some("https://x.com/mcp")
		);
		assert_eq!(
			parsed.lists.get("header").map(Vec::as_slice),
			Some(["A=1".to_string(), "B=2".to_string()].as_slice())
		);
		assert!(parsed.positional.is_empty());

		// Unknown option reports and fails.
		let mut errors = Vec::new();
		let parsed = parse_options(
			&args(&["--wat"]),
			&[("x", None)],
			&mut |line| errors.push(line.to_string()),
			usize::MAX,
		);
		assert!(parsed.is_none());
		assert!(errors[0].contains("Unknown option --wat"));
	}

	#[test]
	fn add_stops_option_parsing_after_two_positionals() {
		let mut error = |_line: &str| panic!("unexpected error");
		let parsed = parse_options(
			&args(&["srv", "bash", "--flag", "extra"]),
			&[("local", None)],
			&mut error,
			2,
		)
		.unwrap();
		// `bash --flag extra` are all positional passthrough.
		assert_eq!(
			parsed.positional,
			vec![
				"srv".to_string(),
				"bash".to_string(),
				"--flag".to_string(),
				"extra".to_string()
			]
		);
	}

	/// One sequential end-to-end pass over the env-dependent paths: the
	/// agent-dir override is process-global, so the sub-scenarios must not
	/// run in parallel.
	#[test]
	fn run_mcp_command_end_to_end() {
		let agent_dir = with_agent_dir("e2e");
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let run = |list: &[&str]| {
			let argv = args(list);
			runtime.block_on(run_mcp_command(&argv))
		};

		// -- list on an empty config.
		assert_eq!(run(&["list"]), 0);

		// -- add: http, replacement, stdio passthrough, bearer header. URLs
		// stay unroutable so the dead-server exit codes are deterministic.
		assert_eq!(
			run(&[
				"add",
				"docs",
				"--url",
				"http://127.0.0.1:1/mcp",
				"--oauth-client-id",
				"my-id"
			]),
			0
		);
		assert_eq!(run(&["add", "docs", "--url", "http://127.0.0.1:1/mcp"]), 0);
		let config: Value =
			serde_json::from_str(&std::fs::read_to_string(mcp_json_path()).unwrap()).unwrap();
		assert_eq!(
			config["mcpServers"]["docs"]["url"],
			"http://127.0.0.1:1/mcp"
		);
		assert!(config["mcpServers"]["docs"].get("oauth").is_none());

		assert_eq!(
			run(&["add", "fs", "--", "bash", "-c", "echo hi", "--verbose"]),
			0
		);
		let config: Value =
			serde_json::from_str(&std::fs::read_to_string(mcp_json_path()).unwrap()).unwrap();
		assert_eq!(config["mcpServers"]["fs"]["command"], "bash");
		assert_eq!(
			config["mcpServers"]["fs"]["args"],
			serde_json::json!(["-c", "echo hi", "--verbose"])
		);

		assert_eq!(
			run(&[
				"add",
				"tok",
				"--url",
				"http://127.0.0.1:1/mcp",
				"--bearer-token-env-var",
				"T"
			]),
			0
		);
		let config: Value =
			serde_json::from_str(&std::fs::read_to_string(mcp_json_path()).unwrap()).unwrap();
		assert_eq!(
			config["mcpServers"]["tok"]["headers"]["Authorization"],
			"Bearer ${T}"
		);

		// -- login/logout error paths.
		assert_eq!(run(&["login", "ghost"]), 1);
		assert_eq!(run(&["login", "fs"]), 1);
		assert_eq!(run(&["logout", "fs"]), 1);

		// -- list: dead servers fail the run; disabled servers do not.
		assert_eq!(run(&["list"]), 1);
		assert_eq!(run(&["list", "--json"]), 1);
		let path = mcp_json_path();
		std::fs::write(
			&path,
			serde_json::json!({
				"mcpServers": {
					"off": {"url": "http://127.0.0.1:1/mcp", "enabled": false}
				}
			})
			.to_string(),
		)
		.unwrap();
		assert_eq!(run(&["list"]), 0);
		assert_eq!(run(&["list", "--json"]), 0);

		// -- logout: no stored credentials → clean zero.
		std::fs::write(
			&path,
			serde_json::json!({"mcpServers": {"docs": {"url": "http://127.0.0.1:1/mcp"}}})
				.to_string(),
		)
		.unwrap();
		assert_eq!(run(&["logout", "docs"]), 0);

		unsafe { std::env::remove_var(crate::config::AGENT_DIR_ENV) };
		drop(agent_dir);
	}
}
