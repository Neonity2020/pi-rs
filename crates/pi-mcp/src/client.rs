//! MCP client — port of `pi/packages/mcp/src/client.ts`.
//!
//! Drives one server transport through initialize → tools/list → tools/call.
//! The coding-agent MCP extension registers each server's tools under the
//! `mcp__<server>__<tool>` namespace.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::protocol::types::{
	CallToolResult, InitializeParams, InitializeResult, ListToolsResult, ServerInfo,
};
use crate::transports::{Transport, TransportEvent};

/// Server entry from `mcp.json` or `pi.registerMcpServer()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpServerConfig {
	Stdio {
		command: String,
		#[serde(default)]
		args: Vec<String>,
		#[serde(default)]
		env: Vec<(String, String)>,
	},
	Http {
		url: String,
		#[serde(default)]
		headers: Vec<(String, String)>,
	},
}

/// Tool metadata as surfaced to the model registry.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
	pub server: String,
	pub name: String,
	/// `mcp__<server>__<name>`
	pub qualified_name: String,
	pub description: Option<String>,
	pub input_schema: Value,
}

pub struct McpClient {
	pub server_name: String,
	transport: Box<dyn Transport>,
	next_id: u64,
	server_info: Option<InitializeResult>,
	started: bool,
}

impl McpClient {
	pub fn new(server_name: impl Into<String>, transport: Box<dyn Transport>) -> Self {
		Self {
			server_name: server_name.into(),
			transport,
			next_id: 0,
			server_info: None,
			started: false,
		}
	}

	pub fn server_info(&self) -> Option<&InitializeResult> {
		self.server_info.as_ref()
	}

	/// initialize handshake, then `notifications/initialized`.
	pub async fn initialize(&mut self, client_version: &str) -> crate::Result<InitializeResult> {
		// Like upstream `connect()`: start the transport before the handshake.
		if !self.started {
			self.transport.start().await?;
			self.started = true;
		}
		let params = InitializeParams {
			protocol_version: "2025-06-18".into(),
			capabilities: serde_json::json!({}),
			client_info: ServerInfo {
				name: "pi-rs".into(),
				version: client_version.into(),
			},
		};
		let result: InitializeResult = self
			.request("initialize", serde_json::to_value(&params)?)
			.await?;
		let notification =
			JsonRpcRequest::notification("notifications/initialized", serde_json::json!({}));
		self.transport.send(&notification).await?;
		self.server_info = Some(result.clone());
		Ok(result)
	}

	/// Close the transport gracefully and wait for the server to exit: the
	/// stdio transport runs the spec shutdown ladder (stdin close → grace →
	/// SIGTERM → SIGKILL of the server's process group) and reports `Closed`
	/// when the child is reaped. Safe to call twice; later calls return
	/// immediately. Dropping the client without `close()` still kills the
	/// process group via the transport's orphan guard — just less gently.
	pub async fn close(&mut self) -> crate::Result<()> {
		self.transport.close().await?;
		// Drain trailing frames until the supervisor reports `Closed` (or the
		// channel ends), so callers can treat the server as fully reaped.
		loop {
			match self.transport.recv().await? {
				None | Some(TransportEvent::Closed) => return Ok(()),
				Some(TransportEvent::Message(_) | TransportEvent::Error(_)) => continue,
			}
		}
	}

	pub async fn list_tools(&mut self) -> crate::Result<Vec<McpToolInfo>> {
		// Follow `nextCursor` through all pages, like upstream `listAll`.
		let mut tools = Vec::new();
		let mut cursors = std::collections::HashSet::new();
		let mut cursor: Option<String> = None;
		loop {
			let params = cursor.as_ref().map(|c| serde_json::json!({ "cursor": c }));
			let page: ListToolsResult = self
				.request("tools/list", params.unwrap_or(Value::Null))
				.await?;
			tools.extend(page.tools);
			let Some(next) = page.next_cursor else {
				break;
			};
			// Some servers end pagination with `null` or `""` instead of omitting the cursor.
			if next.is_empty() || !cursors.insert(next.clone()) {
				break;
			}
			cursor = Some(next);
		}
		let mut taken = std::collections::HashSet::new();
		Ok(tools
			.into_iter()
			.map(|t| {
				let qualified_name =
					crate::tool::create_mcp_tool_name(&self.server_name, &t.name, |name| {
						!taken.insert(name.to_string())
					});
				taken.insert(qualified_name.clone());
				McpToolInfo {
					qualified_name,
					server: self.server_name.clone(),
					name: t.name,
					description: t.description,
					input_schema: t.input_schema,
				}
			})
			.collect())
	}

	/// `tools/call` with the upstream wire shape: `{ name, arguments }`, with
	/// `arguments` omitted when absent (JSON `null`).
	pub async fn call_tool(
		&mut self,
		name: &str,
		arguments: Value,
	) -> crate::Result<CallToolResult> {
		let mut params = serde_json::Map::new();
		params.insert("name".into(), Value::from(name));
		if !arguments.is_null() {
			params.insert("arguments".into(), arguments);
		}
		self.request("tools/call", Value::Object(params)).await
	}

	async fn request<T: for<'de> Deserialize<'de>>(
		&mut self,
		method: &str,
		params: Value,
	) -> crate::Result<T> {
		self.next_id += 1;
		let id = self.next_id;
		let request = JsonRpcRequest {
			jsonrpc: "2.0".into(),
			id: Some(Value::from(id)),
			method: method.into(),
			// `null` params are omitted, matching upstream's absent `params`.
			params: if params.is_null() { None } else { Some(params) },
		};
		self.transport.send(&request).await?;

		loop {
			match self.transport.recv().await? {
				None => return Err(crate::Error::Closed),
				Some(crate::TransportEvent::Closed) => return Err(crate::Error::Closed),
				// Transport errors are report-only upstream: pending requests
				// keep waiting and fail when the transport closes, so a
				// malformed frame does not abort the in-flight request.
				Some(crate::TransportEvent::Error(_)) => continue,
				Some(crate::TransportEvent::Message(frame)) => {
					let frame = *frame;
					if matches!(&frame.id, Some(v) if v.as_u64() == Some(id)) {
						return match (frame.result, frame.error) {
							(Some(result), _) => Ok(serde_json::from_value(result)?),
							(None, Some(error)) => Err(crate::Error::Protocol(format!(
								"{}: {}",
								error.code, error.message
							))),
							(None, None) => {
								Err(crate::Error::Protocol("response without result".into()))
							}
						};
					}
					// Notifications and unrelated frames are skipped for now.
				}
			}
		}
	}
}

/// Helper used by tests: build a response frame.
pub fn response_for(id: u64, result: Value) -> JsonRpcResponse {
	JsonRpcResponse {
		jsonrpc: "2.0".into(),
		id: Some(Value::from(id)),
		result: Some(result),
		error: None,
	}
}

#[cfg(all(test, unix))]
mod close_lifecycle {
	use super::*;
	use std::time::Duration;

	use crate::transports::stdio::StdioTransport;

	/// A server that answers `initialize` and exits on stdin EOF.
	const INIT_ONLY_SERVER: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1.0"}}}\n' "$id" ;;
  esac
done
"#;

	#[tokio::test]
	async fn close_reaps_the_server_and_is_idempotent() {
		let transport =
			StdioTransport::new("sh", vec!["-c".into(), INIT_ONLY_SERVER.into()], vec![]);
		let mut client = McpClient::new("fake", Box::new(transport));
		client.initialize("0.1.0").await.unwrap();
		// The server exits on stdin EOF inside the grace window, so close
		// returns well before the SIGTERM/SIGKILL escalation.
		tokio::time::timeout(Duration::from_secs(10), client.close())
			.await
			.expect("close must not hang")
			.unwrap();
		// A second close is a no-op.
		tokio::time::timeout(Duration::from_secs(5), client.close())
			.await
			.expect("second close must not hang")
			.unwrap();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use tokio::sync::mpsc;

	use crate::transports::in_memory;

	/// Wire the client to a fake server speaking raw JSON lines (the server
	/// side needs the request `method`, which parsed `TransportEvent`s drop).
	async fn wired_client() -> (
		McpClient,
		mpsc::UnboundedReceiver<String>,
		mpsc::UnboundedSender<String>,
	) {
		let (to_server_tx, to_server_rx) = mpsc::unbounded_channel::<String>();
		let (to_client_tx, to_client_rx) = mpsc::unbounded_channel::<String>();
		let transport = in_memory::from_raw("client", to_client_rx, to_server_tx);
		let client = McpClient::new("paged", Box::new(transport));
		(client, to_server_rx, to_client_tx)
	}

	async fn reply(to_client_tx: &mpsc::UnboundedSender<String>, id: u64, result: Value) {
		to_client_tx
			.send(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
			.unwrap();
	}

	#[tokio::test]
	async fn list_tools_follows_next_cursor_pages() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			// Page 1: no params on the wire, `nextCursor` present.
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["method"], "tools/list");
			assert!(
				message.get("params").is_none(),
				"the first page must omit params"
			);
			reply(&to_client_tx, message["id"].as_u64().unwrap(),
				json!({"tools": [{"name": "a", "description": null, "inputSchema": {"type": "object"}}], "nextCursor": "page-2"})).await;
			// Page 2: cursor param, no `nextCursor` → done.
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["params"]["cursor"], "page-2");
			reply(
				&to_client_tx,
				message["id"].as_u64().unwrap(),
				json!({"tools": [{"name": "b", "description": null, "inputSchema": {}}]}),
			)
			.await;
		});

		let tools = client.list_tools().await.unwrap();
		server.await.unwrap();
		let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
		assert_eq!(names, ["a", "b"]);
		assert_eq!(tools[0].qualified_name, "mcp__paged__a");
	}

	#[tokio::test]
	async fn call_tool_sends_the_upstream_wire_shape() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["method"], "tools/call");
			assert_eq!(
				message["params"],
				json!({"name": "echo", "arguments": {"message": "hi"}})
			);
			reply(
				&to_client_tx,
				message["id"].as_u64().unwrap(),
				json!({"content": [{"type": "text", "text": "pong"}], "isError": false}),
			)
			.await;
			// `null` arguments are omitted, like upstream's absent `arguments`.
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["params"], json!({"name": "echo"}));
			reply(&to_client_tx, message["id"].as_u64().unwrap(), json!({})).await;
		});

		let result = client
			.call_tool("echo", json!({"message": "hi"}))
			.await
			.unwrap();
		assert!(
			matches!(&result.content[0], crate::protocol::content::Content::Text { text } if text == "pong")
		);
		client.call_tool("echo", Value::Null).await.unwrap();
		server.await.unwrap();
	}

	#[tokio::test]
	async fn json_rpc_errors_surface_as_protocol_errors() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
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
		});

		let error = client.call_tool("nope", Value::Null).await.unwrap_err();
		server.await.unwrap();
		assert!(
			matches!(error, crate::Error::Protocol(message) if message.contains("Unknown tool"))
		);
	}
}
