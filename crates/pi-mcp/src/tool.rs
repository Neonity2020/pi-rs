//! MCP → [`pi_agent::AgentTool`] bridge — Rust port of the MCP tool adapter in
//! `pi/packages/coding-agent/src/extensions/mcp/tools.ts` (subset: name
//! building, parameter schema normalization, result conversion).
//!
//! [`McpTool`] wraps one server tool so it can go straight into
//! `AgentOptions.tools`: MCP content blocks map onto model-facing text/images
//! (`toLlmContent`), `isError` maps onto `is_error`, and failures of the call
//! itself (transport closed, JSON-RPC error) become error results.
//!
//! `McpClient` drives one request/response match loop, so concurrent
//! executions share it through a mutex instead of pipelining.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use pi_agent::{AgentTool, AgentToolResult, ToolUpdate};
use pi_ai::{ImageContent, TextOrImage};

use crate::client::{McpClient, McpToolInfo};
use crate::protocol::content::{Content, ResourceContents};
use crate::protocol::types::CallToolResult;

/// Client handle shared by a server's tools; request/response matching is
/// serialized through the mutex.
pub type SharedMcpClient = Arc<Mutex<McpClient>>;

/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME_LENGTH: usize = 64;

/// An MCP server tool as a [`pi_agent::AgentTool`], ready for
/// `AgentOptions.tools`.
pub struct McpTool {
	server: String,
	/// Tool name on the MCP wire (`tools/call` target).
	mcp_name: String,
	/// `mcp__<server>__<tool>`, sanitized like upstream `createMcpToolName`.
	tool_name: String,
	description: String,
	parameters: Value,
	client: SharedMcpClient,
}

impl McpTool {
	/// Bridge `info` (from [`McpClient::list_tools`]) through `client`.
	pub fn new(info: McpToolInfo, client: SharedMcpClient) -> Self {
		let description = info
			.description
			.filter(|d| !d.trim().is_empty())
			.unwrap_or_else(|| format!("MCP tool {} from server {}", info.name, info.server));
		Self {
			tool_name: create_mcp_tool_name(&info.server, &info.name, |_| false),
			server: info.server,
			mcp_name: info.name,
			description,
			parameters: to_parameters(info.input_schema),
			client,
		}
	}

	/// The server this tool belongs to.
	pub fn server(&self) -> &str {
		&self.server
	}

	/// The tool name on the MCP wire.
	pub fn mcp_name(&self) -> &str {
		&self.mcp_name
	}

	/// Bridge every tool `list_tools` returns for `client`'s server.
	pub async fn all_for(client: &SharedMcpClient) -> crate::Result<Vec<McpTool>> {
		let infos = {
			let mut client = client.lock().await;
			client.list_tools().await?
		};
		Ok(infos
			.into_iter()
			.map(|info| McpTool::new(info, Arc::clone(client)))
			.collect())
	}
}

/// `mcp__<server>__<tool>`, sanitized and shortened with a hash suffix when
/// too long — mirrors upstream `createMcpToolName`. Everything but
/// `[A-Za-z0-9_]` becomes `_`; `is_taken` reports names used by a different
/// MCP tool (sanitizing can map two tools to one name), which then get the
/// hash suffix.
pub fn create_mcp_tool_name(
	server: &str,
	tool: &str,
	mut is_taken: impl FnMut(&str) -> bool,
) -> String {
	let name: String = format!("mcp__{server}__{tool}")
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || c == '_' {
				c
			} else {
				'_'
			}
		})
		.collect();
	if name.len() <= MAX_TOOL_NAME_LENGTH && !is_taken(&name) {
		return name;
	}
	let hash: String = short_hash(&format!("{server}\0{tool}"))
		.chars()
		.take(8)
		.collect();
	let cut = MAX_TOOL_NAME_LENGTH - hash.len() - 1;
	format!("{}_{}", name.get(..cut).unwrap_or(&name), hash)
}

fn short_hash(input: &str) -> String {
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	input.hash(&mut hasher);
	format!("{:016x}", hasher.finish())
}

/// Tool input schemas must be objects; providers reject object schemas
/// without `type`/`properties` — mirrors upstream `toParameters`.
fn to_parameters(schema: Value) -> Value {
	let Value::Object(schema) = schema else {
		return serde_json::json!({ "type": "object", "properties": {} });
	};
	let mut parameters = schema;
	parameters
		.entry("type".to_string())
		.or_insert(Value::from("object"));
	if !parameters.contains_key("properties") {
		parameters.insert("properties".to_string(), serde_json::json!({}));
	}
	Value::Object(parameters)
}

/// Convert an MCP result like upstream `convertMcpResult` plus
/// `toLlmContent`: text blocks join into one text, images pass through,
/// embedded text resources become text, image resources become images, and
/// other blocks become short placeholders. Without content blocks, structured
/// content becomes JSON. `isError` maps onto `is_error`, with a fallback text
/// when the error carries none.
pub fn convert_tool_result(server: &str, tool: &str, result: &CallToolResult) -> AgentToolResult {
	let mut text = String::new();
	let mut images: Vec<ImageContent> = Vec::new();
	for block in &result.content {
		match block {
			Content::Text { text: block_text } => push_text(&mut text, block_text),
			Content::Image { data, mime_type } => images.push(ImageContent {
				data: data.clone(),
				mime_type: mime_type.clone(),
			}),
			Content::Resource { resource } => match (&resource.text, &resource.blob) {
				// Embedded text resource → its text (`blockToLlmContent`).
				(Some(embedded), _) => push_text(&mut text, embedded),
				// Embedded image resource → an image.
				(None, Some(blob))
					if resource
						.mime_type
						.as_deref()
						.is_some_and(|mime| mime.starts_with("image/")) =>
				{
					images.push(ImageContent {
						data: blob.clone(),
						mime_type: resource.mime_type.clone().expect("mime type checked"),
					});
				}
				_ => push_text(&mut text, &binary_placeholder(resource)),
			},
		}
	}
	if text.is_empty()
		&& images.is_empty()
		&& let Some(structured) = &result.structured_content
	{
		text = serde_json::to_string_pretty(structured).unwrap_or_default();
	}
	if result.is_error && text.is_empty() {
		text = format!("MCP tool {server}/{tool} returned an error");
	}

	let mut content = Vec::new();
	if !text.is_empty() {
		content.push(TextOrImage::text(text));
	}
	content.extend(images.into_iter().map(TextOrImage::Image));

	AgentToolResult {
		content,
		// The whole `CallToolResult` for scripts/UI, like upstream's
		// `structuredContent` — `_meta` does not exist in this port.
		structured_content: serde_json::to_value(result).ok(),
		details: Some(serde_json::json!({ "server": server, "tool": tool })),
		is_error: result.is_error,
		..Default::default()
	}
}

/// Join model-facing text blocks with newlines, like upstream `textOf`.
fn push_text(text: &mut String, piece: &str) {
	if !text.is_empty() {
		text.push('\n');
	}
	text.push_str(piece);
}

/// Model-facing text for an embedded resource that is neither text nor an
/// image — mirrors `blockToLlmContent`.
fn binary_placeholder(resource: &ResourceContents) -> String {
	format!(
		"[binary resource {} ({}) omitted]",
		resource.uri,
		resource.mime_type.as_deref().unwrap_or("unknown type")
	)
}

#[async_trait]
impl AgentTool for McpTool {
	fn name(&self) -> &str {
		&self.tool_name
	}

	fn label(&self) -> &str {
		&self.server
	}

	fn description(&self) -> &str {
		&self.description
	}

	fn parameters_schema(&self) -> Value {
		self.parameters.clone()
	}

	async fn execute(
		&self,
		_tool_call_id: &str,
		params: Value,
		_signal: CancellationToken,
		_on_update: Option<ToolUpdate>,
	) -> AgentToolResult {
		let mut client = self.client.lock().await;
		match client.call_tool(&self.mcp_name, params).await {
			Ok(result) => convert_tool_result(&self.server, &self.mcp_name, &result),
			Err(error) => AgentToolResult::error(format!(
				"MCP tool {}/{} failed: {error}",
				self.server, self.mcp_name
			)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::client::McpToolInfo;
	use crate::transports::in_memory;
	use serde_json::json;
	use tokio::sync::mpsc;

	type Raw = (
		SharedMcpClient,
		mpsc::UnboundedReceiver<String>,
		mpsc::UnboundedSender<String>,
	);

	/// Wire a client to a fake server speaking raw JSON lines (the server side
	/// needs the request `method`, which parsed `TransportEvent`s drop).
	async fn wired_client(server_name: &str) -> Raw {
		let (to_server_tx, to_server_rx) = mpsc::unbounded_channel::<String>();
		let (to_client_tx, to_client_rx) = mpsc::unbounded_channel::<String>();
		let transport = in_memory::from_raw("bridge-client", to_client_rx, to_server_tx);
		let client = McpClient::new(server_name, Box::new(transport));
		(Arc::new(Mutex::new(client)), to_server_rx, to_client_tx)
	}

	async fn reply(to_client_tx: &mpsc::UnboundedSender<String>, id: u64, result: Value) {
		to_client_tx
			.send(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
			.unwrap();
	}

	/// Consume the next `tools/call` request and answer with `result`.
	async fn answer_call(
		to_server_rx: &mut mpsc::UnboundedReceiver<String>,
		to_client_tx: &mpsc::UnboundedSender<String>,
		name: &str,
		result: Value,
	) {
		let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
		assert_eq!(message["method"], "tools/call");
		assert_eq!(message["params"]["name"], name);
		reply(to_client_tx, message["id"].as_u64().unwrap(), result).await;
	}

	fn info(name: &str, input_schema: Value) -> McpToolInfo {
		McpToolInfo {
			server: "srv".into(),
			name: name.into(),
			qualified_name: format!("mcp__srv__{name}"),
			description: None,
			input_schema,
		}
	}

	fn text_of(result: &AgentToolResult) -> String {
		result
			.content
			.iter()
			.filter_map(|block| block.text_content())
			.collect::<Vec<_>>()
			.join("\n")
	}

	#[tokio::test]
	async fn execute_maps_content_iserror_and_remote_errors() {
		let (client, mut to_server_rx, to_client_tx) = wired_client("srv").await;
		let server = tokio::spawn(async move {
			// tools/list
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["method"], "tools/list");
			reply(
				&to_client_tx,
				message["id"].as_u64().unwrap(),
				json!({"tools": [
					{"name": "echo", "description": "Echo it back", "inputSchema": {"type": "object"}},
					{"name": "fail", "description": "Fails", "inputSchema": {"type": "object"}},
					{"name": "remote-error", "description": "Errors on the wire", "inputSchema": {"type": "object"}},
					{"name": "structured", "description": "Structured only", "inputSchema": {"type": "object"}},
					{"name": "image", "description": "Returns an image", "inputSchema": {"type": "object"}}
				]}),
			)
			.await;

			answer_call(
				&mut to_server_rx,
				&to_client_tx,
				"echo",
				json!({"content": [
					{"type": "text", "text": "hello from bridge"},
					{"type": "text", "text": "second line"}
				], "isError": false}),
			)
			.await;
			answer_call(
				&mut to_server_rx,
				&to_client_tx,
				"fail",
				json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
			)
			.await;
			// Remote errors arrive as JSON-RPC error frames; the bridge turns
			// them into error results below.
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["method"], "tools/call");
			to_client_tx
				.send(
					json!({
						"jsonrpc": "2.0",
						"id": message["id"],
						"error": {"code": -32602, "message": "Unknown tool: nope"}
					})
					.to_string(),
				)
				.unwrap();
			answer_call(
				&mut to_server_rx,
				&to_client_tx,
				"structured",
				json!({"structuredContent": {"answer": 42}}),
			)
			.await;
			answer_call(
				&mut to_server_rx,
				&to_client_tx,
				"image",
				json!({"content": [{"type": "image", "data": "aGk=", "mimeType": "image/png"}]}),
			)
			.await;
		});

		let tools = McpTool::all_for(&client).await.unwrap();
		let tool = |name: &str| {
			tools
				.iter()
				.find(|t| t.mcp_name() == name)
				.unwrap_or_else(|| panic!("missing tool {name}"))
		};

		let echo = tool("echo")
			.execute("call-1", json!({}), CancellationToken::new(), None)
			.await;
		assert!(!echo.is_error);
		assert_eq!(text_of(&echo), "hello from bridge\nsecond line");
		assert_eq!(echo.details.as_ref().unwrap()["server"], "srv");
		assert_eq!(echo.details.as_ref().unwrap()["tool"], "echo");
		assert_eq!(echo.structured_content.as_ref().unwrap()["isError"], false);

		let fail = tool("fail")
			.execute("call-2", json!({}), CancellationToken::new(), None)
			.await;
		assert!(fail.is_error);
		assert_eq!(text_of(&fail), "boom");

		let remote = tool("remote-error")
			.execute("call-3", json!({}), CancellationToken::new(), None)
			.await;
		assert!(remote.is_error);
		assert!(text_of(&remote).contains("-32602"), "{}", text_of(&remote));
		assert!(text_of(&remote).contains("Unknown tool"));

		let structured = tool("structured")
			.execute("call-4", json!({}), CancellationToken::new(), None)
			.await;
		assert!(!structured.is_error);
		assert_eq!(text_of(&structured), "{\n  \"answer\": 42\n}");

		let image = tool("image")
			.execute("call-5", json!({}), CancellationToken::new(), None)
			.await;
		assert!(!image.is_error);
		assert!(matches!(
			&image.content[0],
			TextOrImage::Image(pi_ai::ImageContent { data, mime_type })
				if data == "aGk=" && mime_type == "image/png"
		));

		server.await.unwrap();
	}

	#[tokio::test]
	async fn a_closed_transport_yields_an_error_result() {
		let (client, to_server_rx, to_client_tx) = wired_client("srv").await;
		// The peer is gone: both raw channels dropped.
		drop(to_server_rx);
		drop(to_client_tx);
		let tool = McpTool::new(info("echo", json!({"type": "object"})), client);
		let result = tool
			.execute("call-1", json!({}), CancellationToken::new(), None)
			.await;
		assert!(result.is_error);
		assert!(text_of(&result).contains("closed"), "{}", text_of(&result));
	}

	#[tokio::test]
	async fn names_descriptions_and_parameter_schemas_follow_upstream() {
		let (client, to_server_rx, to_client_tx) = wired_client("srv").await;
		drop(to_server_rx);
		drop(to_client_tx);

		let tool = McpTool::new(
			info(
				"echo",
				json!({"properties": {"message": {"type": "string"}}}),
			),
			client,
		);
		assert_eq!(tool.name(), "mcp__srv__echo");
		assert_eq!(tool.label(), "srv");
		// Missing description falls back like upstream.
		assert_eq!(tool.description(), "MCP tool echo from server srv");
		// Schema gets `type` and `properties` defaults.
		assert_eq!(
			tool.parameters_schema(),
			json!({"properties": {"message": {"type": "string"}}, "type": "object"})
		);
	}

	#[test]
	fn create_mcp_tool_name_sanitizes_and_hashes() {
		assert_eq!(
			create_mcp_tool_name("github", "create_issue", |_| false),
			"mcp__github__create_issue"
		);
		assert_eq!(
			create_mcp_tool_name("my server", "do.action", |_| false),
			"mcp__my_server__do_action"
		);
		// A taken name gets a hash suffix.
		let renamed = create_mcp_tool_name("srv", "echo", |name| name == "mcp__srv__echo");
		assert_ne!(renamed, "mcp__srv__echo");
		assert!(renamed.starts_with("mcp__srv__echo_"));
		assert_eq!(renamed.len(), "mcp__srv__echo_".len() + 8);
		// Overlong names are cut to 64 characters with a hash suffix.
		let long = create_mcp_tool_name("srv", &"a".repeat(100), |_| false);
		assert_eq!(long.len(), MAX_TOOL_NAME_LENGTH);
		assert!(long.starts_with("mcp__srv__"));
	}

	#[test]
	fn error_results_without_text_get_a_fallback() {
		let result = CallToolResult {
			content: vec![],
			structured_content: None,
			is_error: true,
		};
		let converted = convert_tool_result("srv", "silent", &result);
		assert!(converted.is_error);
		assert_eq!(text_of(&converted), "MCP tool srv/silent returned an error");
	}

	#[tokio::test]
	async fn call_failures_become_error_results() {
		// A server that answers every tools/call with a JSON-RPC error.
		let (client, mut to_server_rx, to_client_tx) = wired_client("srv").await;
		tokio::spawn(async move {
			loop {
				let Some(line) = to_server_rx.recv().await else {
					return;
				};
				let message: Value = serde_json::from_str(&line).unwrap();
				if message["method"] == "tools/call" {
					to_client_tx
						.send(
							json!({
								"jsonrpc": "2.0",
								"id": message["id"],
								"error": {"code": -32000, "message": "server exploded"}
							})
							.to_string(),
						)
						.unwrap();
				}
			}
		});
		let tool = McpTool::new(info("echo", json!({"type": "object"})), client);
		let result = tool
			.execute("call-1", json!({}), CancellationToken::new(), None)
			.await;
		assert!(result.is_error);
		assert!(
			text_of(&result).contains("server exploded"),
			"{}",
			text_of(&result)
		);
	}
}
