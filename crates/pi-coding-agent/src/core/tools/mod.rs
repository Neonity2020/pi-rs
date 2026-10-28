//! Built-in tool registry — port of `pi/packages/coding-agent/src/core/tools/`.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use pi_agent::{AgentTool, AgentToolResult};
use pi_mcp::transports::stdio::StdioTransport;
use pi_mcp::transports::streamable_http::StreamableHttpTransport;
use pi_mcp::{McpClient, McpTool, SharedMcpClient};

use crate::core::mcp_config::{McpConfig, McpServerEntry};

pub const ALL_TOOL_NAMES: &[&str] = &["read", "write", "edit", "bash", "grep", "find", "ls"];

/// `createTools()` — build the active tool set for a working directory.
pub fn create_tools(cwd: impl Into<PathBuf>) -> Vec<Arc<dyn AgentTool>> {
	let cwd = cwd.into();
	vec![
		Arc::new(read::ReadTool::new(cwd.clone())),
		Arc::new(write::WriteTool::new(cwd.clone())),
		Arc::new(edit::EditTool::new(cwd.clone())),
		Arc::new(bash::BashTool::new(cwd.clone())),
		Arc::new(grep::GrepTool::new(cwd.clone())),
		Arc::new(find::FindTool::new(cwd.clone())),
		Arc::new(ls::LsTool::new(cwd)),
	]
}

// -------------------------------------------------------------------------------------------
// MCP server assembly — the startup wiring of the upstream MCP extension
// (`pi/packages/coding-agent/src/extensions/mcp/index.ts`, `session_start`
// handler), reduced to the synchronous connect path: load the global
// `mcp.json`, connect every enabled stdio server, bridge its tools into the
// tool table (`create_tools` result). Failure isolation follows upstream: one
// server failing (spawn error, initialize timeout, protocol error) becomes a
// notice while the rest continue; all servers failing still yields a
// startup-ready tool set.
// -------------------------------------------------------------------------------------------

/// Budget for one server's startup (spawn + initialize + list_tools),
/// mirroring the upstream MCP extension's `startupWaitMs` default of 10s.
pub const MCP_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound per server for the graceful close ladder in
/// [`McpAssembly::shutdown`].
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Connected MCP servers: the bridged tools plus the clients they run
/// against. Main keeps the assembly alive for the session and calls
/// [`McpAssembly::shutdown`] on exit (upstream `session_shutdown`).
pub struct McpAssembly {
	/// Bridged `mcp__<server>__<tool>` tools, ready for `AgentOptions.tools`.
	pub tools: Vec<Arc<dyn AgentTool>>,
	/// Shared clients behind the tools; kept here so the server processes
	/// stay owned until shutdown.
	pub clients: Vec<SharedMcpClient>,
	/// One human-readable line per config error and per server outcome.
	pub notices: Vec<String>,
}

impl McpAssembly {
	/// Run every transport's graceful close ladder (stdin close → grace →
	/// SIGTERM → SIGKILL of the server's process group) and wait for the
	/// child to be reaped, bounded so process exit cannot hang on a stuck
	/// server. Dropping the assembly without `shutdown` still reaps the
	/// groups via the transports' orphan guards — just less gently.
	pub async fn shutdown(&self) {
		for client in &self.clients {
			let mut client = client.lock().await;
			let _ = tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, client.close()).await;
		}
	}
}

/// Connect every enabled stdio server in `config` and bridge its tools.
pub async fn connect_mcp_servers(config: &McpConfig) -> McpAssembly {
	connect_mcp_servers_with(config, MCP_START_TIMEOUT).await
}

/// [`connect_mcp_servers`] with an explicit per-server startup budget (tests
/// shrink it).
pub async fn connect_mcp_servers_with(config: &McpConfig, timeout: Duration) -> McpAssembly {
	let mut assembly = McpAssembly {
		tools: Vec::new(),
		clients: Vec::new(),
		notices: Vec::new(),
	};
	// Config problems first, like upstream `reportProblems`.
	for error in &config.errors {
		assembly.notices.push(format!("config: {error}"));
	}
	for entry in &config.servers {
		if !entry.enabled {
			continue; // Parsed but unconnected, like a disabled server upstream.
		}
		match tokio::time::timeout(timeout, connect_one(entry)).await {
			Ok(Ok((client, tools))) => {
				assembly.notices.push(format!(
					"{name}: connected ({summary})",
					name = entry.name,
					summary = tool_summary(&tools)
				));
				assembly.tools.extend(
					tools
						.into_iter()
						.map(|tool| Arc::new(tool) as Arc<dyn AgentTool>),
				);
				assembly.clients.push(client);
			}
			Ok(Err(error)) => assembly
				.notices
				.push(format!("{}: failed: {error}", entry.name)),
			Err(_) => assembly.notices.push(format!(
				"{}: failed: startup timed out after {}s",
				entry.name,
				timeout.as_secs()
			)),
		}
	}
	assembly
}

/// `N tools` with the first few `mcp__<server>__<tool>` names spelled out —
/// the startup notice doubles as the smoke signal that the bridged tools
/// reached the tool table.
fn tool_summary(tools: &[McpTool]) -> String {
	match tools.len() {
		0 => "0 tools".to_string(),
		1 => format!("1 tool: {}", tools[0].name()),
		_ => {
			const SHOWN: usize = 3;
			let names: Vec<String> = tools
				.iter()
				.take(SHOWN)
				.map(|t| t.name().to_string())
				.collect();
			let more = tools.len() - SHOWN;
			let ellipsis = if more > 0 {
				format!(", … +{more} more")
			} else {
				String::new()
			};
			format!("{} tools: {}{}", tools.len(), names.join(", "), ellipsis)
		}
	}
}

/// One server: stdio transport → client → initialize handshake → bridge the
/// tool table (`McpServerConnection` upstream, startup path only). The
/// transport owns the child's process group, so a dropped future (startup
/// timeout) or a dropped client cleans the server up.
async fn connect_one(entry: &McpServerEntry) -> anyhow::Result<(SharedMcpClient, Vec<McpTool>)> {
	let transport: Box<dyn pi_mcp::Transport> = if let Some(url) = &entry.url {
		Box::new(StreamableHttpTransport::new(url, entry.headers.clone()))
	} else {
		let mut transport =
			StdioTransport::new(entry.command.clone(), entry.args.clone(), entry.env.clone());
		if let Some(cwd) = &entry.cwd {
			transport = transport.with_cwd(cwd);
		}
		Box::new(transport)
	};
	let mut client = McpClient::new(entry.name.clone(), transport);
	client.initialize(crate::config::VERSION).await?;
	let client: SharedMcpClient = Arc::new(tokio::sync::Mutex::new(client));
	let tools = McpTool::all_for(&client).await?;
	Ok((client, tools))
}

/// Resolve a tool-provided path against the tool cwd, mirroring
/// `path-utils.ts` (absolute paths pass through, `~` expands).
pub fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
	let expanded = if let Some(rest) = path.strip_prefix("~/") {
		dirs::home_dir().unwrap_or_default().join(rest)
	} else {
		PathBuf::from(path)
	};
	if expanded.is_absolute() {
		expanded
	} else {
		cwd.join(expanded)
	}
}

/// Extract a required string parameter. The error is boxed because
/// `AgentToolResult` is large and this helper sits on hot tool paths.
pub fn param_str(params: &Value, key: &str) -> Result<String, Box<AgentToolResult>> {
	params
		.get(key)
		.and_then(|v| v.as_str())
		.map(|s| s.to_string())
		.ok_or_else(|| {
			Box::new(AgentToolResult::error(format!(
				"missing required string parameter: {key}"
			)))
		})
}

/// Extract an optional parameter of mixed type.
pub fn param_value(params: &Value, key: &str) -> Option<Value> {
	params.get(key).cloned()
}

pub fn param_str_or(params: &Value, key: &str, default: &str) -> String {
	params
		.get(key)
		.and_then(|v| v.as_str())
		.unwrap_or(default)
		.to_string()
}

pub fn param_u64_or(params: &Value, key: &str, default: u64) -> u64 {
	params.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}

pub fn param_bool_or(params: &Value, key: &str, default: bool) -> bool {
	params.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

#[cfg(test)]
mod mcp_assembly_tests {
	use super::*;
	use crate::core::mcp_config::load_mcp_config_from;
	use serde_json::json;
	use tokio_util::sync::CancellationToken;

	/// Fake MCP server over `sh -c` — the same script the pi-mcp stdio
	/// transport tests use: answers initialize, tools/list, and tools/call
	/// with one `echo` tool.
	const FAKE_SERVER: &str = r#"printf 'fake-mcp ready\n' >&2
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo a message","inputSchema":{"type":"object","properties":{"message":{"type":"string"}}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hello from fake server"}],"isError":false}}\n' "$id" ;;
  esac
done
"#;

	fn write_config(tag: &str, config: Value) -> PathBuf {
		let path = std::env::temp_dir().join(format!(
			"pi-rs-mcp-assembly-{}-{}-{}.json",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::write(&path, config.to_string()).unwrap();
		path
	}

	fn remove_config(path: &PathBuf) {
		let _ = std::fs::remove_file(path);
	}

	fn tool_named<'a>(tools: &'a [Arc<dyn AgentTool>], name: &str) -> &'a Arc<dyn AgentTool> {
		tools
			.iter()
			.find(|tool| tool.name() == name)
			.unwrap_or_else(|| panic!("missing tool {name}"))
	}

	fn text_of(result: &AgentToolResult) -> String {
		result
			.content
			.iter()
			.filter_map(|block| block.text_content())
			.collect::<Vec<_>>()
			.join("\n")
	}

	/// The full startup path through a config file: MCP tools join the
	/// `create_tools` result (system prompt + tool table), and one call
	/// round-trips to the fake server and back.
	#[tokio::test]
	async fn mcp_tools_join_the_table_and_roundtrip() {
		let path = write_config(
			"roundtrip",
			json!({"mcpServers": {"fake": {"command": "sh", "args": ["-c", FAKE_SERVER]}}}),
		);
		let config = load_mcp_config_from(&path).await;
		remove_config(&path);

		let mut assembly = connect_mcp_servers(&config).await;
		assert!(
			assembly.notices.iter().any(|n| n.contains("connected")),
			"{:?}",
			assembly.notices
		);

		let mut tools = create_tools(std::env::temp_dir());
		tools.extend(std::mem::take(&mut assembly.tools));
		assert!(
			tools.iter().any(|t| t.name() == "read"),
			"built-ins survive the merge"
		);
		let echo = tool_named(&tools, "mcp__fake__echo");
		assert_eq!(echo.description(), "Echo a message");

		let result = echo
			.execute(
				"call-1",
				json!({"message": "hi"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		assert_eq!(text_of(&result), "hello from fake server");

		// Shutdown runs the graceful close ladder; afterwards the tool
		// reports the closed transport instead of hanging.
		assembly.shutdown().await;
		let after = echo
			.execute("call-2", json!({}), CancellationToken::new(), None)
			.await;
		assert!(after.is_error);
		assert!(text_of(&after).contains("closed"), "{}", text_of(&after));
	}

	#[tokio::test]
	async fn http_config_connects_and_calls_tool() {
		use std::io::{Read, Write};
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			for stream in listener.incoming().take(5) {
				let mut stream = stream.unwrap();
				stream
					.set_read_timeout(Some(std::time::Duration::from_secs(5)))
					.unwrap();
				let mut bytes = Vec::new();
				loop {
					let mut chunk = [0; 4096];
					let n = stream.read(&mut chunk).unwrap();
					if n == 0 {
						break;
					}
					bytes.extend_from_slice(&chunk[..n]);
					if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
						let head = String::from_utf8_lossy(&bytes[..end]);
						let len = head
							.lines()
							.find_map(|l| {
								l.to_ascii_lowercase()
									.strip_prefix("content-length: ")
									.and_then(|n| n.parse::<usize>().ok())
							})
							.unwrap_or(0);
						if bytes.len() >= end + 4 + len {
							break;
						}
					}
				}
				let request = String::from_utf8(bytes).unwrap();
				assert!(request.to_ascii_lowercase().contains("x-test: secret"));
				let (status, body) = if request.starts_with("GET ") {
					("405 Method Not Allowed", String::new())
				} else {
					let body = request.split_once("\r\n\r\n").unwrap().1;
					let frame: Value = serde_json::from_str(body).unwrap();
					let id = frame["id"].clone();
					match frame["method"].as_str().unwrap() {
						"notifications/initialized" => ("202 Accepted", String::new()),
						"initialize" => ("200 OK", json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}).to_string()),
						"tools/list" => ("200 OK", json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}).to_string()),
						"tools/call" => ("200 OK", json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"HTTP works"}]}}).to_string()),
						other => panic!("unexpected method {other}"),
					}
				};
				write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
			}
		});
		let path = write_config(
			"http",
			json!({"mcpServers":{"web":{"url":url,"headers":{"X-Test":"secret"}}}}),
		);
		let config = load_mcp_config_from(&path).await;
		remove_config(&path);
		assert!(config.errors.is_empty(), "{:?}", config.errors);
		let assembly = connect_mcp_servers(&config).await;
		assert!(
			assembly
				.notices
				.iter()
				.any(|notice| notice.contains("connected")),
			"{:?}",
			assembly.notices
		);
		let result = assembly.tools[0]
			.execute("id", json!({}), CancellationToken::new(), None)
			.await;
		assert_eq!(text_of(&result), "HTTP works");
		assembly.shutdown().await;
		server.join().unwrap();
	}

	#[tokio::test]
	async fn a_broken_server_does_not_block_the_good_one() {
		let path = write_config(
			"partial-failure",
			json!({
				"mcpServers": {
					"broken": { "command": "/nonexistent/pi-rs-mcp-server" },
					"fake": { "command": "sh", "args": ["-c", FAKE_SERVER] }
				}
			}),
		);
		let config = load_mcp_config_from(&path).await;
		remove_config(&path);

		let mut assembly = connect_mcp_servers(&config).await;
		assert!(
			assembly
				.notices
				.iter()
				.any(|n| n.starts_with("broken: failed:")),
			"{:?}",
			assembly.notices
		);
		assert!(
			assembly
				.notices
				.iter()
				.any(|n| n.starts_with("fake: connected")),
			"{:?}",
			assembly.notices
		);
		// The good server's tools are in the table despite the broken one.
		let mut tools = create_tools(std::env::temp_dir());
		tools.extend(std::mem::take(&mut assembly.tools));
		tool_named(&tools, "mcp__fake__echo");
		assembly.shutdown().await;
	}

	#[tokio::test]
	async fn all_servers_failing_still_yields_a_startup_ready_toolset() {
		let path = write_config(
			"all-fail",
			json!({
				"mcpServers": {
					"missing": { "command": "/nonexistent/pi-rs-mcp-server" },
					"garbage": { "command": "false" }
				}
			}),
		);
		let config = load_mcp_config_from(&path).await;
		remove_config(&path);

		let assembly = connect_mcp_servers(&config).await;
		assert!(assembly.tools.is_empty() && assembly.clients.is_empty());
		assert_eq!(assembly.notices.len(), 2, "{:?}", assembly.notices);
		// The CLI would start with the built-in tool set only.
		let tools = create_tools(std::env::temp_dir());
		assert_eq!(tools.len(), 7);
		assembly.shutdown().await;
	}

	#[tokio::test]
	async fn a_hanging_server_hits_the_startup_budget() {
		let path = write_config(
			"hanging",
			json!({"mcpServers": {"sleepy": {"command": "sh", "args": ["-c", "sleep 30"]}}}),
		);
		let config = load_mcp_config_from(&path).await;
		remove_config(&path);

		// Shrunk budget: `sleep 30` never answers the initialize handshake.
		let assembly = connect_mcp_servers_with(&config, Duration::from_secs(1)).await;
		assert!(assembly.tools.is_empty());
		assert!(
			assembly
				.notices
				.iter()
				.any(|n| n.contains("sleepy: failed: startup timed out after 1s")),
			"{:?}",
			assembly.notices
		);
		// Dropping the assembly (without shutdown) kills the timed-out
		// server's process group via the transport's orphan guard.
		drop(assembly);
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}
