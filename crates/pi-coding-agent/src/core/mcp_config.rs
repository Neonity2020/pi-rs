//! MCP server configuration — port of
//! `pi/packages/coding-agent/src/extensions/mcp/config.ts` (global `mcp.json`
//! reading) plus the server-entry validation of
//! `pi/packages/coding-agent/src/core/mcp-servers.ts`.
//!
//! Upstream reads `mcp.json` from the agent directory (and, for trusted
//! projects, `<project>/.pi/mcp.json` — not implemented here) in the
//! `mcpServers` shape shared with other MCP clients, so existing
//! configurations can be copied over:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
//!     "docs": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ..." } }
//!   }
//! }
//! ```
//!
//! This build implements stdio and streamable HTTP servers, plus
//! `enabled: false` to keep an entry without connecting. Unsupported forms
//! are reported as non-fatal config errors: HTTP OAuth/auth,
//! the legacy `sse` type, and the exposure
//! pipeline (`exposure`/`toolExposure` for codemode/tool_search — tolerated
//! as unknown fields). Unknown fields are ignored so configs written for
//! other clients load. Env values that reference variables (`${NAME}`) or
//! commands (`!cmd`) are passed through literally; expansion lands with the
//! upstream env resolution.
//!
//! A missing file is the default state (empty config, no notice); a broken or
//! empty file yields an empty config plus error strings that main surfaces as
//! startup notices — config problems never panic and never block the CLI.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;

/// `<agent-dir>/mcp.json`, next to the settings file — upstream keeps the
/// same file name in the agent directory.
pub fn mcp_json_path() -> PathBuf {
	config::agent_dir().join("mcp.json")
}

/// One validated stdio server entry — upstream `McpStdioServerConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
	pub name: String,
	pub url: Option<String>,
	pub headers: Vec<(String, String)>,
	pub command: String,
	pub args: Vec<String>,
	/// Extra environment entries; the child inherits the parent environment
	/// and these overlay it (upstream `inheritEnv: true`).
	pub env: Vec<(String, String)>,
	/// Working directory for the server process.
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

/// Load `<agent-dir>/mcp.json`.
pub fn load_mcp_config() -> McpConfig {
	load_mcp_config_from(&mcp_json_path())
}

/// Read one `mcp.json` — upstream `readConfigFile` for the global scope.
pub fn load_mcp_config_from(path: &Path) -> McpConfig {
	let mut config = McpConfig::default();
	let Ok(text) = std::fs::read_to_string(path) else {
		// Missing file: nothing configured, nothing to report.
		return config;
	};
	let text = text.strip_prefix('\u{feff}').unwrap_or(&text); // stripBom
	let parsed: Value = match serde_json::from_str(text) {
		Ok(value) => value,
		Err(error) => {
			config.errors.push(format!("{}: {error}", path.display()));
			return config;
		}
	};
	let failure = || {
		format!(
			"{}: expected an object with an \"mcpServers\" object",
			path.display()
		)
	};
	let Some(object) = parsed.as_object() else {
		config.errors.push(failure());
		return config;
	};
	let Some(servers) = object.get("mcpServers").map(|servers| servers.as_object()) else {
		return config;
	};
	let Some(servers) = servers else {
		config.errors.push(failure());
		return config;
	};
	for (name, value) in servers {
		match validate_server(name, value) {
			Ok(entry) => {
				// Names that differ only in `-` and `_` would share a tool
				// namespace (upstream `mcpNamespace` clash check).
				let clash = config.servers.iter().find(|other| {
					other.name != entry.name
						&& mcp_namespace(&other.name) == mcp_namespace(&entry.name)
				});
				if let Some(clash) = clash {
					config.errors.push(format!(
						"{}: server \"{name}\" conflicts with \"{}\"",
						path.display(),
						clash.name
					));
				} else {
					config.servers.push(entry);
				}
			}
			Err(error) => config.errors.push(format!("{}: {error}", path.display())),
		}
	}
	config
}

/// `mcp__<server>` with `-` replaced by `_`, like the tool names — upstream
/// `mcpNamespace`.
fn mcp_namespace(server: &str) -> String {
	format!("mcp__{}", server.replace('-', "_"))
}

/// Validate one `mcpServers` entry — upstream `validateMcpServerConfig`,
/// reduced to the stdio branch; URL servers are documented in the module docs
/// and rejected with a dedicated message.
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
		let config = load_mcp_config_from(&temp_path("missing"));
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
					}
				},
				"autoEnableCodemode": false,
				"unknownTopLevel": [1, 2]
			})
			.to_string(),
		)
		.unwrap();

		let config = load_mcp_config_from(&path);
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		assert_eq!(config.servers.len(), 2);

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
			let config = load_mcp_config_from(&path);
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
		let config = load_mcp_config_from(&path);
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
		let config = load_mcp_config_from(&path);
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
		let config = load_mcp_config_from(&path);
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
