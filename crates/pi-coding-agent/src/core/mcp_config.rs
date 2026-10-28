//! MCP server configuration — port of
//! `pi/packages/coding-agent/src/extensions/mcp/config.ts` (global + project
//! `mcp.json` reading and merging), the server-entry validation of
//! `pi/packages/coding-agent/src/core/mcp-servers.ts`, and the value
//! resolution of `pi/packages/coding-agent/src/core/resolve-config-value.ts`.
//!
//! Upstream reads `mcp.json` from the agent directory and, for trusted
//! projects, from `<project>/.pi/mcp.json`, in the `mcpServers` shape shared
//! with other MCP clients, so existing configurations can be copied over:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
//!     "docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" } }
//!   }
//! }
//! ```
//!
//! This build implements stdio (`command`/`args`/`env`/`cwd`) and streamable
//! HTTP (`url`/`headers`) servers, plus `enabled: false` to keep an entry
//! without connecting. Unsupported forms are reported as non-fatal config
//! errors, like a validation failure upstream: HTTP `oauth`/`auth` (pi-rs has
//! no OAuth client, so `auth` is rejected in both scopes — upstream allows it
//! in the global file only), the legacy `sse` type, and the exposure pipeline
//! (`exposure`/`toolExposure` for codemode/tool_search — tolerated as unknown
//! fields). Unknown fields are ignored so configs written for other clients
//! load.
//!
//! Project merge (upstream `readConfigFile`): a project entry carrying
//! `command`, `url`, or `type` replaces the global server with the same name
//! wholesale; an entry without any of them overrides only `enabled` of the
//! global server — upstream also allows overriding `exposure`/`toolExposure`,
//! which pi-rs does not implement, so those keys are reported as errors. An
//! override without a global base is an error, and the `-`/`_` namespace
//! clash check runs over the merged set. Loading is gated on project trust
//! (`crate::core::trust`).
//!
//! Env/header value resolution (upstream `resolveConfigValue`): values may
//! reference environment variables (`${NAME}`, `$NAME`) or shell commands
//! (`!cmd`); `$$` escapes a literal `$` and `$!` a literal `!`; a variable
//! missing from the (optional) config env map falls back to the process
//! environment, and any missing variable fails the whole value. Resolution
//! differences from upstream: commands run through tokio (`sh -c`, 10s
//! timeout) instead of the synchronous `execSync`, and command results are
//! cached per config load rather than for the process lifetime.
//! `command`/`args`/`cwd` are never `${}`-expanded; a leading `~` in `cwd`
//! expands to the home directory. Resolution is fail-fast per entry: an
//! unresolved value drops that entry with a config error, never a panic.
//!
//! A missing file is the default state (empty config, no notice); a broken or
//! empty file yields an empty config plus error strings that main surfaces as
//! startup notices — config problems never panic and never block the CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config;

/// `<agent-dir>/mcp.json`, next to the settings file — upstream keeps the
/// same file name in the agent directory.
pub fn mcp_json_path() -> PathBuf {
	config::agent_dir().join("mcp.json")
}

/// One validated server entry — upstream `McpStdioServerConfig` (a stdio
/// entry: `command` set, `url` `None`) or `McpHttpServerConfig` (`url` set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
	pub name: String,
	pub url: Option<String>,
	/// Raw header values; `${VAR}`/`!cmd` references resolve at load end.
	pub headers: Vec<(String, String)>,
	pub command: String,
	pub args: Vec<String>,
	/// Raw env values; the child inherits the parent environment, these
	/// overlay it (upstream `inheritEnv: true`), and `${VAR}`/`!cmd`
	/// references resolve at load end.
	pub env: Vec<(String, String)>,
	/// Working directory for the server process; a leading `~` expands.
	pub cwd: Option<String>,
	/// `enabled: false` keeps the entry parsed but unconnected, like upstream.
	pub enabled: bool,
}

/// Loaded configuration — upstream `LoadedMcpConfig`, reduced to the server
/// entries plus the error list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpConfig {
	pub servers: Vec<McpServerEntry>,
	/// Non-fatal problems: unreadable/invalid JSON and per-entry validation
	/// failures. Surfaced as startup notices, then ignored.
	pub errors: Vec<String>,
}

/// Load the agent-dir config plus — when the project is trusted —
/// `<cwd>/.pi/mcp.json`, merged, with env/header values resolved. Async
/// because a `!cmd` value spawns a process.
pub async fn load_mcp_config() -> McpConfig {
	let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
	let project_trusted = crate::core::trust::is_trusted(&cwd);
	load_mcp_config_with(&cwd, project_trusted).await
}

/// [`load_mcp_config`] with the trust decision supplied — main asks before
/// loading and passes the answer; tests inject both.
pub async fn load_mcp_config_with(cwd: &Path, project_trusted: bool) -> McpConfig {
	load_mcp_config_paths(&mcp_json_path(), cwd, project_trusted).await
}

/// Merge core with injectable paths — upstream `loadMcpConfig(options)`.
pub async fn load_mcp_config_paths(global: &Path, cwd: &Path, project_trusted: bool) -> McpConfig {
	let mut state = ConfigState::default();
	read_config_file(global, Scope::Global, &mut state);
	if project_trusted {
		read_config_file(
			&cwd.join(".pi").join("mcp.json"),
			Scope::Project,
			&mut state,
		);
	}
	resolve_entries(&mut state).await;
	McpConfig {
		servers: state.servers,
		errors: state.errors,
	}
}

/// Read one `mcp.json` in the global scope with values resolved — the
/// single-file form for callers that do not merge projects.
pub async fn load_mcp_config_from(path: &Path) -> McpConfig {
	load_mcp_config_paths(path, Path::new("/"), false).await
}

/// Which file an entry is being read from — upstream `readConfigFile`'s
/// `scope` argument.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
	Global,
	Project,
}

/// Accumulated servers and errors across both files — upstream
/// `McpConfigState` (a name map, kept as an ordered vec here).
#[derive(Default)]
struct ConfigState {
	servers: Vec<McpServerEntry>,
	errors: Vec<String>,
}

/// Read one `mcp.json` into `state` — upstream `readConfigFile`. Missing
/// files are silent; parse and validation failures accumulate in
/// `state.errors`.
fn read_config_file(path: &Path, scope: Scope, state: &mut ConfigState) {
	let Ok(text) = std::fs::read_to_string(path) else {
		// Missing file: nothing configured, nothing to report.
		return;
	};
	let text = text.strip_prefix('\u{feff}').unwrap_or(&text); // stripBom
	let parsed: Value = match serde_json::from_str(text) {
		Ok(value) => value,
		Err(error) => {
			state.errors.push(format!("{}: {error}", path.display()));
			return;
		}
	};
	let failure = || {
		format!(
			"{}: expected an object with an \"mcpServers\" object",
			path.display()
		)
	};
	let Some(object) = parsed.as_object() else {
		state.errors.push(failure());
		return;
	};
	let Some(servers) = object.get("mcpServers").and_then(Value::as_object) else {
		if object.contains_key("mcpServers") {
			state.errors.push(failure());
		}
		return;
	};
	for (name, value) in servers {
		read_server_entry(path, scope, name, value, state);
	}
}

/// Validate and merge one `mcpServers` entry — the per-entry body of upstream
/// `readConfigFile`.
fn read_server_entry(
	path: &Path,
	scope: Scope,
	name: &str,
	value: &Value,
	state: &mut ConfigState,
) {
	let mut fail = |message: String| state.errors.push(format!("{}: {message}", path.display()));
	// A project entry without command/url/type overrides a global server
	// instead of defining one — upstream `isOverride` + `OVERRIDE_KEYS`,
	// reduced to `enabled` (pi-rs has no exposure/toolExposure to override).
	if scope == Scope::Project && is_override(value) {
		let object = value.as_object().expect("is_override implies an object");
		let Some(base) = state.servers.iter_mut().find(|server| server.name == name) else {
			fail(format!(
				"server \"{name}\" needs \"command\" or \"url\", or a global server to override"
			));
			return;
		};
		if object.keys().any(|key| key != "enabled") {
			fail(format!(
				"server \"{name}\": an override can only set enabled"
			));
			return;
		}
		match object.get("enabled") {
			None => {}
			Some(Value::Bool(enabled)) => base.enabled = *enabled,
			Some(_) => fail(format!("server \"{name}\": enabled must be a boolean")),
		}
		return;
	}
	let entry = match validate_server(name, value) {
		Ok(entry) => entry,
		Err(error) => {
			fail(error);
			return;
		}
	};
	// Names that differ only in `-` and `_` would share a tool namespace —
	// checked over the merged set, so global/project names collide too
	// (upstream `mcpNamespace` clash check).
	let clash = state.servers.iter().find(|other| {
		other.name != entry.name && mcp_namespace(&other.name) == mcp_namespace(&entry.name)
	});
	if let Some(clash) = clash {
		fail(format!(
			"server \"{name}\" conflicts with \"{}\"",
			clash.name
		));
		return;
	}
	// Project entries replace same-name global ones wholesale (upstream
	// `servers.set`).
	match state.servers.iter().position(|server| server.name == name) {
		Some(position) => state.servers[position] = entry,
		None => state.servers.push(entry),
	}
}

/// Whether an entry overrides a server defined elsewhere instead of defining
/// one — upstream `isOverride`: no `command`, `url`, or `type`.
fn is_override(value: &Value) -> bool {
	value.as_object().is_some_and(|object| {
		!object.contains_key("command")
			&& !object.contains_key("url")
			&& !object.contains_key("type")
	})
}

/// Resolve `${VAR}`/`!cmd` values in stdio `env` and HTTP `headers`, and
/// expand a leading `~` in `cwd`. Fail-fast per entry: one unresolved value
/// drops the whole entry with a config error.
async fn resolve_entries(state: &mut ConfigState) {
	let mut resolver = Resolver::new(None);
	let servers = std::mem::take(&mut state.servers);
	for mut entry in servers {
		match resolve_entry_values(&mut resolver, &mut entry).await {
			Ok(()) => state.servers.push(entry),
			Err(message) => state
				.errors
				.push(format!("server \"{}\": {message}", entry.name)),
		}
	}
}

async fn resolve_entry_values(
	resolver: &mut Resolver,
	entry: &mut McpServerEntry,
) -> Result<(), String> {
	for (key, value) in &mut entry.env {
		let description = format!("env \"{key}\"");
		*value = resolver
			.resolve(value)
			.await
			.map_err(|error| resolve_error_message(&description, error))?;
	}
	for (key, value) in &mut entry.headers {
		let description = format!("header \"{key}\"");
		*value = resolver
			.resolve(value)
			.await
			.map_err(|error| resolve_error_message(&description, error))?;
	}
	if let Some(cwd) = entry.cwd.take() {
		entry.cwd = Some(expand_tilde(&cwd));
	}
	Ok(())
}

/// Upstream `resolveConfigValueOrThrow`'s error wording.
fn resolve_error_message(description: &str, error: ResolveError) -> String {
	match error {
		ResolveError::Command(command) => {
			format!("failed to resolve {description} from shell command: {command}")
		}
		ResolveError::MissingVars(names) if names.len() == 1 => format!(
			"failed to resolve {description} from environment variable: {}",
			names[0]
		),
		ResolveError::MissingVars(names) => format!(
			"failed to resolve {description} from environment variables: {}",
			names.join(", ")
		),
	}
}

/// `~` and `~/…` expand to the home directory; anything else passes through.
/// No `${}` expansion here (upstream resolves relative cwd against the
/// session directory at spawn; pi-rs spawns from the process cwd).
fn expand_tilde(path: &str) -> String {
	let home =
		|rest: &str| dirs::home_dir().map(|home| home.join(rest).to_string_lossy().into_owned());
	if let Some(rest) = path.strip_prefix("~/") {
		home(rest).unwrap_or_else(|| path.to_string())
	} else if path == "~" {
		home("").unwrap_or_else(|| path.to_string())
	} else {
		path.to_string()
	}
}

/// `mcp__<server>` with `-` replaced by `_`, like the tool names — upstream
/// `mcpNamespace`.
fn mcp_namespace(server: &str) -> String {
	format!("mcp__{}", server.replace('-', "_"))
}

/// Validate one `mcpServers` entry — upstream `validateMcpServerConfig`,
/// reduced to the stdio and streamable-HTTP branches. `auth`/`oauth` are
/// rejected in every scope (no OAuth client in this build).
fn validate_server(name: &str, value: &Value) -> Result<McpServerEntry, String> {
	if name.is_empty()
		|| !name
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
	{
		return Err(format!(
			"invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
		));
	}
	let Some(object) = value.as_object() else {
		return Err(format!("server \"{name}\" must be an object"));
	};
	if object.get("type").and_then(Value::as_str) == Some("sse") {
		return Err(format!(
			"server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
		));
	}
	if let Some(url) = object.get("url").and_then(Value::as_str) {
		if !(url.starts_with("https://") || url.starts_with("http://")) {
			return Err(format!("server \"{name}\": URL must use http or https"));
		}
		if object.contains_key("oauth") || object.contains_key("auth") {
			return Err(format!(
				"server \"{name}\": HTTP OAuth/auth is not supported"
			));
		}
		let headers = match object.get("headers") {
			None => Vec::new(),
			Some(Value::Object(map)) if map.values().all(Value::is_string) => map
				.iter()
				.map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
				.collect(),
			_ => {
				return Err(format!(
					"server \"{name}\": headers must map names to strings"
				));
			}
		};
		let enabled = match object.get("enabled") {
			None => true,
			Some(value) => value
				.as_bool()
				.ok_or_else(|| format!("server \"{name}\": enabled must be a boolean"))?,
		};
		return Ok(McpServerEntry {
			name: name.to_string(),
			url: Some(url.to_string()),
			headers,
			command: String::new(),
			args: Vec::new(),
			env: Vec::new(),
			cwd: None,
			enabled,
		});
	}
	let Some(command) = object.get("command").and_then(Value::as_str) else {
		return Err(format!(
			"server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
		));
	};
	let args = match object.get("args") {
		None => Vec::new(),
		Some(value) => match value.as_array() {
			Some(items) if items.iter().all(Value::is_string) => items
				.iter()
				.map(|item| item.as_str().expect("checked").to_string())
				.collect(),
			_ => {
				return Err(format!(
					"server \"{name}\": args must be an array of strings"
				));
			}
		},
	};
	let env = match object.get("env") {
		None => Vec::new(),
		Some(value) => match value.as_object() {
			Some(map) if map.values().all(Value::is_string) => map
				.iter()
				.map(|(key, value)| (key.clone(), value.as_str().expect("checked").to_string()))
				.collect(),
			_ => return Err(format!("server \"{name}\": env must map names to strings")),
		},
	};
	let cwd = match object.get("cwd") {
		None => None,
		Some(value) => match value.as_str() {
			Some(cwd) => Some(cwd.to_string()),
			None => return Err(format!("server \"{name}\": cwd must be a string")),
		},
	};
	let enabled = match object.get("enabled") {
		None => true,
		Some(value) => match value.as_bool() {
			Some(enabled) => enabled,
			None => return Err(format!("server \"{name}\": enabled must be a boolean")),
		},
	};
	Ok(McpServerEntry {
		name: name.to_string(),
		url: None,
		headers: Vec::new(),
		command: command.to_string(),
		args,
		env,
		cwd,
		enabled,
	})
}

// -------------------------------------------------------------------------------------------
// Config value resolution — port of
// `pi/packages/coding-agent/src/core/resolve-config-value.ts`.
// -------------------------------------------------------------------------------------------

/// One parsed template part — upstream `TemplatePart`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
	Literal(String),
	Env(String),
}

/// Why a value failed to resolve — upstream reports these through thrown
/// errors in `resolveConfigValueOrThrow`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolveError {
	/// `!cmd` failed: non-zero exit, timeout, spawn failure, or empty output.
	Command(String),
	/// Environment variables missing from both the config map and the
	/// process environment, in first-reference order.
	MissingVars(Vec<String>),
}

/// Resolver over one config load — the upstream module-level
/// `commandResultCache`, scoped to a load instead of the process lifetime.
struct Resolver {
	/// Config-provided env map consulted before the process environment.
	env: Option<HashMap<String, String>>,
	/// `!cmd` results (including failures) keyed by the raw config string.
	command_cache: HashMap<String, Option<String>>,
}

impl Resolver {
	fn new(env: Option<HashMap<String, String>>) -> Self {
		Self {
			env,
			command_cache: HashMap::new(),
		}
	}

	/// Resolve one value — upstream `resolveConfigValue`: `!cmd` runs a shell
	/// command, otherwise `${VAR}`/`$NAME` interpolate (with `$$`/`$!`
	/// escapes) and any missing variable fails the value.
	async fn resolve(&mut self, config: &str) -> Result<String, ResolveError> {
		if let Some(command) = config.strip_prefix('!') {
			if let Some(cached) = self.command_cache.get(config) {
				return cached
					.clone()
					.ok_or_else(|| ResolveError::Command(command.to_string()));
			}
			let value = execute_command(config).await;
			self.command_cache.insert(config.to_string(), value.clone());
			return value.ok_or_else(|| ResolveError::Command(command.to_string()));
		}
		let parts = parse_template(config);
		let mut resolved = String::new();
		let mut missing: Vec<String> = Vec::new();
		for part in &parts {
			match part {
				Part::Literal(value) => resolved.push_str(value),
				Part::Env(name) => match lookup_env(name, self.env.as_ref()) {
					Some(value) => resolved.push_str(&value),
					None => {
						if !missing.iter().any(|seen| seen == name) {
							missing.push(name.clone());
						}
					}
				},
			}
		}
		if missing.is_empty() {
			Ok(resolved)
		} else {
			Err(ResolveError::MissingVars(missing))
		}
	}
}

/// Upstream `resolveEnvConfigValue` including its `||` quirk: an empty string
/// in the config map falls through to the process environment, and an empty
/// process variable counts as missing.
fn lookup_env(name: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
	match env
		.and_then(|map| map.get(name))
		.filter(|value| !value.is_empty())
	{
		Some(value) => Some(value.clone()),
		None => std::env::var(name).ok().filter(|value| !value.is_empty()),
	}
}

/// Run a `!cmd` value: `sh -c` with stdin null, 10s budget, stdout trimmed —
/// upstream `executeWithDefaultShell` (`execSync`, stdio ignore/pipe/ignore).
/// Any failure, including empty output, resolves to `None`.
async fn execute_command(command_config: &str) -> Option<String> {
	let command = command_config.strip_prefix('!').unwrap_or(command_config);
	let run = async {
		let output = tokio::process::Command::new("sh")
			.arg("-c")
			.arg(command)
			.stdin(std::process::Stdio::null())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::null())
			.output()
			.await
			.ok()?;
		if !output.status.success() {
			return None;
		}
		let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
		(!value.is_empty()).then_some(value)
	};
	tokio::time::timeout(Duration::from_secs(10), run)
		.await
		.unwrap_or(None)
}

/// `^[A-Za-z_][A-Za-z0-9_]*$` — upstream `ENV_VAR_NAME_RE`.
fn is_env_name(name: &str) -> bool {
	let mut chars = name.chars();
	let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
	first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Longest `^[A-Za-z_][A-Za-z0-9_]*` prefix — upstream `ENV_VAR_NAME_PREFIX_RE`.
fn env_name_prefix(value: &str) -> Option<&str> {
	let mut end = 0;
	for (index, c) in value.char_indices() {
		let allowed = if index == 0 {
			c.is_ascii_alphabetic() || c == '_'
		} else {
			c.is_ascii_alphanumeric() || c == '_'
		};
		if !allowed {
			break;
		}
		end = index + c.len_utf8();
	}
	(end > 0).then_some(&value[..end])
}

/// Parse a template value — upstream `parseConfigValueTemplate`: `${VAR}` and
/// `$VAR` reference variables, `$$`/`$!` escape literals, and anything else
/// (`${1A}`, unterminated `${`, bare `$`) stays literal.
fn parse_template(config: &str) -> Vec<Part> {
	let bytes = config.as_bytes();
	let mut parts: Vec<Part> = Vec::new();
	let mut index = 0usize;
	while index < config.len() {
		let Some(offset) = config[index..].find('$') else {
			parts.push(Part::Literal(config[index..].to_string()));
			break;
		};
		let dollar = index + offset;
		if dollar > index {
			parts.push(Part::Literal(config[index..dollar].to_string()));
		}
		// All index arithmetic anchors on ASCII bytes, so the slices below
		// are always on char boundaries.
		match bytes.get(dollar + 1) {
			Some(b'$') | Some(b'!') => {
				parts.push(Part::Literal(
					(*bytes.get(dollar + 1).unwrap() as char).to_string(),
				));
				index = dollar + 2;
			}
			Some(b'{') => match config[dollar + 2..].find('}') {
				None => {
					parts.push(Part::Literal("$".into()));
					index = dollar + 1;
				}
				Some(end_offset) => {
					let end = dollar + 2 + end_offset;
					let name = &config[dollar + 2..end];
					if is_env_name(name) {
						parts.push(Part::Env(name.to_string()));
					} else {
						parts.push(Part::Literal(config[dollar..=end].to_string()));
					}
					index = end + 1;
				}
			},
			_ => match env_name_prefix(&config[dollar + 1..]) {
				Some(name) => {
					parts.push(Part::Env(name.to_string()));
					index = dollar + 1 + name.len();
				}
				None => {
					parts.push(Part::Literal("$".into()));
					index = dollar + 1;
				}
			},
		}
	}
	parts
}

#[cfg(test)]
mod resolve_tests {
	use super::*;

	fn resolved(config: &str, env: Option<&[(&str, &str)]>) -> Result<String, ResolveError> {
		let map = env.map(|entries| {
			entries
				.iter()
				.map(|(k, v)| (k.to_string(), v.to_string()))
				.collect::<HashMap<_, _>>()
		});
		// No `!cmd` in these cases, so the async shell path is never entered.
		let mut resolver = Resolver::new(map);
		futures::executor::block_on(resolver.resolve(config))
	}

	#[test]
	fn literals_pass_through() {
		assert_eq!(resolved("plain-token", None).unwrap(), "plain-token");
		assert_eq!(resolved("", None).unwrap(), "");
	}

	#[test]
	fn brace_and_bare_forms_interpolate_from_the_map() {
		assert_eq!(
			resolved("${TOKEN}", Some(&[("TOKEN", "v1")])).unwrap(),
			"v1"
		);
		assert_eq!(resolved("$TOKEN", Some(&[("TOKEN", "v2")])).unwrap(), "v2");
		assert_eq!(
			resolved("Bearer $ALT_KEY!", Some(&[("ALT_KEY", "k")])).unwrap(),
			"Bearer k!"
		);
		assert_eq!(
			resolved("${A}-${B}", Some(&[("A", "1"), ("B", "2")])).unwrap(),
			"1-2"
		);
	}

	#[test]
	fn escapes_and_non_names_stay_literal() {
		assert_eq!(resolved("$$x", None).unwrap(), "$x");
		assert_eq!(resolved("$!x", None).unwrap(), "!x");
		assert_eq!(resolved("${1A}", None).unwrap(), "${1A}");
		assert_eq!(resolved("$1ABC", None).unwrap(), "$1ABC");
		assert_eq!(resolved("a$", None).unwrap(), "a$");
		assert_eq!(resolved("100$", None).unwrap(), "100$");
		assert_eq!(resolved("$9x", None).unwrap(), "$9x");
		assert_eq!(resolved("${UNCLOSED", None).unwrap(), "${UNCLOSED");
	}

	#[test]
	fn missing_variables_fail_the_whole_value() {
		let error = resolved("${A} and ${B}", Some(&[("A", "1")])).unwrap_err();
		assert_eq!(
			resolve_error_message("env \"K\"", error),
			"failed to resolve env \"K\" from environment variable: B"
		);
		let error = resolved("$A $B $C", None).unwrap_err();
		assert_eq!(
			resolve_error_message("header \"H\"", error),
			"failed to resolve header \"H\" from environment variables: A, B, C"
		);
		// Repeated references report once, in first order.
		let error = resolved("$B $A $B", None).unwrap_err();
		assert!(matches!(error, ResolveError::MissingVars(ref names) if names == &["B", "A"]));
	}

	#[test]
	fn empty_map_values_fall_through_like_upstream() {
		// Upstream `env?.[name] || process.env[name]`: "" in the map is falsy.
		let name = format!("PI_RS_RESOLVE_FALLTHROUGH_{}", std::process::id());
		// SAFETY: test-only process env write; the name is unique per process,
		// and the test process env is not read concurrently by other tests.
		unsafe { std::env::set_var(&name, "from-process") };
		assert_eq!(
			resolved(&format!("${{{name}}}"), Some(&[(name.as_str(), "")])).unwrap(),
			"from-process"
		);
		assert_eq!(
			resolved(&format!("${{{name}}}"), None).unwrap(),
			"from-process"
		);
		// SAFETY: see above.
		unsafe { std::env::remove_var(&name) };
	}

	#[tokio::test]
	async fn commands_run_trimmed_and_are_cached_per_resolver() {
		let marker = std::env::temp_dir().join(format!(
			"pi-rs-resolve-cache-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let _ = std::fs::remove_file(&marker);
		// stdout is trimmed; `>> marker` runs exactly once if (and only if)
		// the shell command executes.
		let config = format!("!printf '  token  ' && echo run >> {}", marker.display());
		let mut resolver = Resolver::new(None);
		let first = resolver.resolve(&config).await.unwrap();
		assert_eq!(first, "token");
		// The second resolve hits the cache: the command must not re-run.
		let second = resolver.resolve(&config).await.unwrap();
		assert_eq!(second, "token");
		assert_eq!(
			std::fs::read_to_string(&marker).unwrap(),
			"run\n",
			"the command ran once"
		);
		let _ = std::fs::remove_file(&marker);
	}

	#[tokio::test]
	async fn failing_and_empty_commands_are_errors() {
		let mut resolver = Resolver::new(None);
		let error = resolver.resolve("!exit 3").await.unwrap_err();
		assert_eq!(
			resolve_error_message("env \"K\"", error),
			"failed to resolve env \"K\" from shell command: exit 3"
		);
		// Empty trimmed output counts as a failure, like upstream.
		assert!(resolver.resolve("!true").await.is_err());
		// A shell syntax error (spawn succeeds, non-zero exit) too.
		assert!(
			resolver
				.resolve("!definitely-not-a-command-π-rs")
				.await
				.is_err()
		);
	}

	#[test]
	fn env_name_matching_follows_the_upstream_rules() {
		assert!(is_env_name("A_1"));
		assert!(!is_env_name("1A"));
		assert!(!is_env_name(""));
		assert_eq!(env_name_prefix("ABC-"), Some("ABC"));
		assert_eq!(env_name_prefix("-A"), None);
		assert_eq!(env_name_prefix(""), None);
	}
}

#[cfg(test)]
mod merge_tests {
	use super::*;
	use serde_json::json;

	/// A throwaway "agent dir" config path plus project cwd.
	struct Fixture {
		dir: PathBuf,
	}

	impl Fixture {
		fn new(tag: &str) -> Self {
			let dir = std::env::temp_dir().join(format!(
				"pi-rs-mcp-merge-{tag}-{}-{}",
				std::process::id(),
				pi_ai::types::now_ms()
			));
			std::fs::create_dir_all(dir.join(".pi")).unwrap();
			Self { dir }
		}

		fn global(&self) -> PathBuf {
			self.dir.join("mcp.json")
		}

		fn project(&self) -> PathBuf {
			self.dir.join(".pi").join("mcp.json")
		}

		fn write_global(&self, value: Value) {
			std::fs::write(self.global(), value.to_string()).unwrap();
		}

		fn write_project(&self, value: Value) {
			std::fs::write(self.project(), value.to_string()).unwrap();
		}

		fn load(&self, trusted: bool) -> McpConfig {
			futures::executor::block_on(load_mcp_config_paths(&self.global(), &self.dir, trusted))
		}
	}

	impl Drop for Fixture {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.dir);
		}
	}

	#[test]
	fn project_entries_replace_global_ones_wholesale() {
		let fixture = Fixture::new("replace");
		fixture.write_global(json!({
			"mcpServers": {
				"shared": { "command": "global-cmd", "args": ["--global"], "env": {"A": "1"} },
				"global-only": { "command": "keep" }
			}
		}));
		fixture.write_project(json!({
			"mcpServers": {
				"shared": { "command": "project-cmd", "cwd": "/project" }
			}
		}));
		let config = fixture.load(true);
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		assert_eq!(config.servers.len(), 2);
		let shared = config.servers.iter().find(|s| s.name == "shared").unwrap();
		assert_eq!(shared.command, "project-cmd");
		assert_eq!(
			shared.args,
			Vec::<String>::new(),
			"global fields do not survive"
		);
		assert!(shared.env.is_empty());
		assert_eq!(shared.cwd.as_deref(), Some("/project"));
		assert!(config.servers.iter().any(|s| s.name == "global-only"));
	}

	#[test]
	fn enabled_only_overrides_flip_the_global_flag() {
		let fixture = Fixture::new("override");
		fixture.write_global(json!({
			"mcpServers": {
				"svc": { "command": "global-cmd", "args": ["--keep"], "enabled": true }
			}
		}));
		fixture.write_project(json!({
			"mcpServers": { "svc": { "enabled": false } }
		}));
		let config = fixture.load(true);
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		let svc = &config.servers[0];
		assert_eq!(svc.command, "global-cmd", "the global entry is kept");
		assert_eq!(svc.args, vec!["--keep"]);
		assert!(!svc.enabled, "only the flag flips");
	}

	#[test]
	fn bad_overrides_are_errors() {
		let fixture = Fixture::new("bad-override");
		fixture.write_global(json!({
			"mcpServers": { "svc": { "command": "x" }, "svc2": { "command": "y" } }
		}));
		fixture.write_project(json!({
			"mcpServers": {
				"stranger": { "enabled": false },
				"svc": { "enabled": false, "cwd": "/nope" },
				"svc2": { "enabled": "yes" }
			}
		}));
		let config = fixture.load(true);
		let find = |needle: &str| {
			config
				.errors
				.iter()
				.find(|message| message.contains(needle))
				.unwrap_or_else(|| panic!("missing `{needle}` in {:?}", config.errors))
		};
		find("server \"stranger\" needs \"command\" or \"url\", or a global server to override");
		find("server \"svc\": an override can only set enabled");
		find("server \"svc2\": enabled must be a boolean");
		// The good global entries survive untouched.
		assert_eq!(config.servers.len(), 2);
		assert!(config.servers.iter().all(|server| server.enabled));
	}

	#[test]
	fn untrusted_projects_are_ignored_entirely() {
		let fixture = Fixture::new("untrusted");
		fixture.write_global(json!({ "mcpServers": { "g": { "command": "x" } } }));
		fixture.write_project(json!({ "mcpServers": { "p": { "command": "y" } } }));
		let config = fixture.load(false);
		assert!(config.errors.is_empty());
		assert_eq!(config.servers.len(), 1);
		assert_eq!(config.servers[0].name, "g");
	}

	#[test]
	fn namespace_clashes_across_files_are_rejected() {
		let fixture = Fixture::new("cross-clash");
		fixture.write_global(json!({ "mcpServers": { "a-b": { "command": "x" } } }));
		fixture.write_project(json!({ "mcpServers": { "a_b": { "command": "y" } } }));
		let config = fixture.load(true);
		assert_eq!(config.servers.len(), 1);
		assert!(config.errors[0].contains("server \"a_b\" conflicts with \"a-b\""));
	}

	#[test]
	fn project_auth_is_rejected_and_errors_accumulate_across_files() {
		let fixture = Fixture::new("auth");
		fixture.write_global(json!({
			"mcpServers": { "broken-global": { "command": 7 } }
		}));
		fixture.write_project(json!({
			"mcpServers": {
				"docs": { "url": "https://example.com/mcp", "auth": { "provider": "acme" } }
			}
		}));
		let config = fixture.load(true);
		// One error from each file: errors accumulate across the merge.
		assert_eq!(config.errors.len(), 2, "{:?}", config.errors);
		assert!(config.errors[0].contains("broken-global"));
		assert!(
			config.errors[1].contains("server \"docs\"") && config.errors[1].contains("auth"),
			"{:?}",
			config.errors
		);
		assert!(config.servers.is_empty());
	}

	#[test]
	fn env_values_resolve_in_merged_entries() {
		let fixture = Fixture::new("env");
		let name = format!("PI_RS_MERGE_ENV_{}", std::process::id());
		fixture.write_global(json!({
			"mcpServers": {
				"stdio": { "command": "sh", "args": ["-c", "env"], "env": { "TOKEN": format!("${{{name}}}") } },
				"http": { "url": "https://example.com/mcp", "headers": { "Authorization": format!("Bearer ${{{name}}}") } }
			}
		}));
		// An enabled-only override must not disturb the resolved values.
		fixture.write_project(json!({
			"mcpServers": { "http": { "enabled": true } }
		}));
		// SAFETY: test-only process env write with a per-process name.
		unsafe { std::env::set_var(&name, "resolved-value") };
		let config = futures::executor::block_on(async {
			let mut state = ConfigState::default();
			read_config_file(&fixture.global(), Scope::Global, &mut state);
			read_config_file(&fixture.project(), Scope::Project, &mut state);
			resolve_entries(&mut state).await;
			McpConfig {
				servers: state.servers,
				errors: state.errors,
			}
		});
		// SAFETY: see above.
		unsafe { std::env::remove_var(&name) };
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		let stdio = config.servers.iter().find(|s| s.name == "stdio").unwrap();
		assert_eq!(stdio.env, vec![("TOKEN".into(), "resolved-value".into())]);
		let http = config.servers.iter().find(|s| s.name == "http").unwrap();
		assert_eq!(
			http.headers,
			vec![("Authorization".into(), "Bearer resolved-value".into())]
		);
	}

	#[test]
	fn unresolved_env_values_fail_their_entry_fast() {
		let fixture = Fixture::new("env-fail");
		fixture.write_global(json!({
			"mcpServers": {
				"good": { "command": "x" },
				"bad": { "command": "x", "env": { "TOKEN": "${DEFINITELY_MISSING_VAR_XY}" } }
			}
		}));
		let config = fixture.load(true);
		assert_eq!(config.servers.len(), 1);
		assert_eq!(config.servers[0].name, "good");
		assert!(
			config.errors[0].contains("server \"bad\"")
				&& config.errors[0].contains("DEFINITELY_MISSING_VAR_XY"),
			"{:?}",
			config.errors
		);
	}

	#[test]
	fn cwd_expands_tilde_but_not_variables() {
		let fixture = Fixture::new("cwd");
		fixture.write_global(json!({
			"mcpServers": {
				"home": { "command": "x", "cwd": "~/sub" },
				"plain": { "command": "x", "cwd": "${HOME}/sub" }
			}
		}));
		let config = fixture.load(true);
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		let home = config.servers.iter().find(|s| s.name == "home").unwrap();
		assert!(
			home.cwd
				.as_deref()
				.is_some_and(|cwd| cwd.ends_with("/sub") && cwd.starts_with('/')),
			"{:?}",
			home.cwd
		);
		let plain = config.servers.iter().find(|s| s.name == "plain").unwrap();
		assert_eq!(plain.cwd.as_deref(), Some("${HOME}/sub"));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn temp_path(tag: &str) -> PathBuf {
		let path = std::env::temp_dir().join(format!(
			"pi-rs-mcp-config-{}-{}-{}.json",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let _ = std::fs::remove_file(&path);
		path
	}

	#[test]
	fn missing_file_is_an_empty_config_without_errors() {
		let config = futures::executor::block_on(load_mcp_config_from(&temp_path("missing")));
		assert_eq!(config, McpConfig::default());
	}

	#[test]
	fn a_good_file_parses_entries_and_keeps_field_order() {
		let path = temp_path("good");
		std::fs::write(
			&path,
			json!({
				"mcpServers": {
					"plain": { "command": "fake-cmd" },
					"full": {
						"command": "sh",
						"args": ["-c", "echo hi"],
						"env": { "B": "2", "A": "1" },
						"cwd": "/tmp",
						"enabled": false,
						// Unknown fields from other clients and unimplemented
						// upstream features are tolerated.
						"exposure": "direct",
						"timeout": 30,
						"future": { "nested": true }
					},
					"web": { "url": "https://example.com/mcp", "headers": { "X-Auth": "Bearer t" } }
				},
				"autoEnableCodemode": false,
				"unknownTopLevel": [1, 2]
			})
			.to_string(),
		)
		.unwrap();

		let config = futures::executor::block_on(load_mcp_config_from(&path));
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		assert_eq!(config.servers.len(), 3);

		let plain = &config.servers[0];
		assert_eq!(plain.name, "plain");
		assert_eq!(plain.command, "fake-cmd");
		assert!(plain.args.is_empty() && plain.env.is_empty() && plain.cwd.is_none());
		assert!(plain.enabled);

		let full = &config.servers[1];
		assert_eq!(full.name, "full");
		assert_eq!(full.args, vec!["-c", "echo hi"]);
		assert_eq!(
			full.env,
			vec![("B".into(), "2".into()), ("A".into(), "1".into())]
		);
		assert_eq!(full.cwd.as_deref(), Some("/tmp"));
		assert!(!full.enabled);

		let web = &config.servers[2];
		assert_eq!(web.name, "web");
		assert_eq!(web.url.as_deref(), Some("https://example.com/mcp"));
		assert_eq!(web.headers, vec![("X-Auth".into(), "Bearer t".into())]);
		assert_eq!(web.command, "");
		assert!(web.enabled);
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn broken_empty_and_misshaped_files_are_errors_not_panics() {
		let cases: [(&str, &str); 4] = [
			("json", "{not json"),
			("empty", ""),
			("array", "[]"),
			("servers-type", "{\"mcpServers\": 7}"),
		];
		for (tag, text) in cases {
			let path = temp_path(tag);
			std::fs::write(&path, text).unwrap();
			let config = futures::executor::block_on(load_mcp_config_from(&path));
			assert_eq!(config.servers.len(), 0, "{tag}");
			assert_eq!(config.errors.len(), 1, "{tag}: {:?}", config.errors);
			assert!(
				config.errors[0].starts_with(path.to_string_lossy().as_ref()),
				"{tag}"
			);
			let _ = std::fs::remove_file(&path);
		}
		// A BOM-prefixed file still parses (upstream stripBom).
		let path = temp_path("bom");
		std::fs::write(&path, "\u{feff}{\"mcpServers\":{}}").unwrap();
		let config = futures::executor::block_on(load_mcp_config_from(&path));
		assert!(config.errors.is_empty() && config.servers.is_empty());
	}

	#[test]
	fn per_entry_validation_mirrors_upstream() {
		let path = temp_path("invalid");
		std::fs::write(
			&path,
			json!({
				"mcpServers": {
					"bad name": { "command": "x" },
					"not-object": "x",
					"http": { "url": "https://example.com/mcp" },
					"sse": { "type": "sse", "url": "https://example.com/mcp" },
					"no-transport": { "enabled": true },
					"bad-args": { "command": "x", "args": [1] },
					"bad-env": { "command": "x", "env": { "A": 1 } },
					"bad-cwd": { "command": "x", "cwd": 7 },
					"bad-enabled": { "command": "x", "enabled": "yes" },
					"good": { "command": "x" }
				}
			})
			.to_string(),
		)
		.unwrap();
		let config = futures::executor::block_on(load_mcp_config_from(&path));
		assert_eq!(config.servers.len(), 2);
		assert!(config.servers.iter().any(|s| s.name == "good"));
		assert!(
			config
				.servers
				.iter()
				.any(|s| s.name == "http" && s.url.as_deref() == Some("https://example.com/mcp"))
		);
		let messages: Vec<String> = config.errors;
		let find = |needle: &str| {
			messages
				.iter()
				.find(|message| message.contains(needle))
				.unwrap_or_else(|| panic!("missing `{needle}` in {messages:?}"))
		};
		find("invalid server name \"bad name\"");
		find("server \"not-object\" must be an object");
		find("server \"sse\": legacy SSE transport is not supported");
		find("server \"no-transport\" needs either \"command\"");
		find("server \"bad-args\": args must be an array of strings");
		find("server \"bad-env\": env must map names to strings");
		find("server \"bad-cwd\": cwd must be a string");
		find("server \"bad-enabled\": enabled must be a boolean");
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn names_clashing_only_in_dashes_are_rejected() {
		let path = temp_path("clash");
		std::fs::write(
			&path,
			json!({
				"mcpServers": {
					"a-b": { "command": "x" },
					"a_b": { "command": "y" }
				}
			})
			.to_string(),
		)
		.unwrap();
		let config = futures::executor::block_on(load_mcp_config_from(&path));
		assert_eq!(config.servers.len(), 1);
		assert!(config.errors[0].contains("server \"a_b\" conflicts with \"a-b\""));
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn namespace_follows_the_tool_name_shape() {
		assert_eq!(mcp_namespace("my-server"), "mcp__my_server");
		assert_eq!(mcp_namespace("srv"), "mcp__srv");
	}
}
