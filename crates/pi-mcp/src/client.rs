//! MCP client — port of `pi/packages/mcp/src/client.ts`.
//!
//! Drives one server transport through initialize → tools/list → tools/call.
//! The coding-agent MCP extension registers each server's tools under the
//! `mcp__<server>__<tool>` namespace.
//!
//! Port deviations from the upstream `McpClient`:
//! - One request in flight at a time (the request loop matches responses by
//!   id). Server → client requests (`ping`, …) are answered while a request is
//!   being awaited; frames arriving outside that window are not consumed.
//! - Per-request timeout with `notifications/cancelled` mirrors upstream's
//!   default (30 s) and wire shape, but abort signals and progress callbacks
//!   are not modeled.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::protocol::types::{
	CallToolResult, InitializeParams, InitializeResult, ListToolsResult, ServerInfo,
	ToolAnnotations, ToolExecution,
};
use crate::transports::{Transport, TransportEvent};

/// Upstream `DEFAULT_REQUEST_TIMEOUT_MS` (`client.ts`).
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// JSON-RPC error code for unhandled server → client methods.
const METHOD_NOT_FOUND: i64 = -32601;

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

/// Tool metadata as surfaced to the model registry. The optional fields pass
/// the server's tool descriptors through uninterpreted (upstream `Tool`
/// fields the bridge does not act on).
#[derive(Debug, Clone, Default)]
pub struct McpToolInfo {
	pub server: String,
	pub name: String,
	/// `mcp__<server>__<name>`
	pub qualified_name: String,
	pub description: Option<String>,
	pub input_schema: Value,
	pub title: Option<String>,
	pub output_schema: Option<Value>,
	pub annotations: Option<ToolAnnotations>,
	pub execution: Option<ToolExecution>,
	pub meta: Option<Value>,
}

pub struct McpClient {
	pub server_name: String,
	transport: Box<dyn Transport>,
	next_id: u64,
	server_info: Option<InitializeResult>,
	started: bool,
	/// The server-selected protocol version after a successful `initialize`.
	protocol_version: Option<String>,
	/// Per-request timeout; `None` uses [`DEFAULT_REQUEST_TIMEOUT_MS`], `Some(0)`
	/// disables the timeout like upstream's non-finite/non-positive values.
	request_timeout_ms: Option<u64>,
}

impl McpClient {
	pub fn new(server_name: impl Into<String>, transport: Box<dyn Transport>) -> Self {
		Self {
			server_name: server_name.into(),
			transport,
			next_id: 0,
			server_info: None,
			started: false,
			protocol_version: None,
			request_timeout_ms: None,
		}
	}

	/// Override the per-request timeout (`McpClientOptions.requestTimeoutMs`).
	pub fn with_request_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
		self.request_timeout_ms = timeout_ms;
		self
	}

	/// The effective per-request timeout in milliseconds; `0` disables it.
	pub fn request_timeout_ms(&self) -> u64 {
		self.request_timeout_ms
			.unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS)
	}

	pub fn server_info(&self) -> Option<&InitializeResult> {
		self.server_info.as_ref()
	}

	/// The protocol version the server selected during `initialize`.
	pub fn protocol_version(&self) -> Option<&str> {
		self.protocol_version.as_deref()
	}

	/// initialize handshake, then `notifications/initialized`.
	pub async fn initialize(&mut self, client_version: &str) -> crate::Result<InitializeResult> {
		// Like upstream `connect()`: start the transport before the handshake.
		if !self.started {
			self.transport.start().await?;
			self.started = true;
		}
		let params = InitializeParams {
			protocol_version: crate::protocol::types::LATEST_PROTOCOL_VERSION.into(),
			capabilities: serde_json::json!({}),
			client_info: ServerInfo {
				name: "pi-rs".into(),
				version: client_version.into(),
			},
		};
		let result: InitializeResult = self
			.request("initialize", serde_json::to_value(&params)?)
			.await?;
		// Servers that do not know the requested version answer with their own
		// latest; anything outside the supported set is a hard failure and
		// closes the connection (upstream `client.ts` connect()).
		if !crate::protocol::types::is_supported_protocol_version(&result.protocol_version) {
			let error = crate::Error::Protocol(format!(
				"MCP server selected unsupported protocol version {}",
				result.protocol_version
			));
			let _ = self.close().await;
			return Err(error);
		}
		self.protocol_version = Some(result.protocol_version.clone());
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
				Some(
					TransportEvent::Message(_)
					| TransportEvent::Request(_)
					| TransportEvent::Notification(_)
					| TransportEvent::Error(_),
				) => continue,
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
					title: t.title,
					output_schema: t.output_schema,
					annotations: t.annotations,
					execution: t.execution,
					meta: t.meta,
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

	/// Liveness probe — upstream `McpClient.ping()`.
	pub async fn ping(&mut self) -> crate::Result<()> {
		let _: Value = self.request("ping", Value::Null).await?;
		Ok(())
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

		// One deadline for the whole wait, like upstream's armed timer (only
		// progress notifications re-arm it, which this port does not model).
		let timeout_ms = self.request_timeout_ms();
		let deadline = if timeout_ms > 0 {
			Some(tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms))
		} else {
			None
		};
		loop {
			let event = match deadline {
				Some(deadline) => {
					match tokio::time::timeout_at(deadline, self.transport.recv()).await {
						Ok(event) => event?,
						Err(_elapsed) => {
							// Tell the server to stop the work — except for
							// `initialize`, which the spec forbids cancelling
							// (upstream `cancelPending`).
							if method != "initialize" {
								let cancelled = JsonRpcRequest::notification(
									"notifications/cancelled",
									serde_json::json!({
										"requestId": id,
										"reason": "Request timed out"
									}),
								);
								self.transport.send(&cancelled).await?;
							}
							return Err(crate::Error::Timeout(timeout_ms));
						}
					}
				}
				None => self.transport.recv().await?,
			};
			match event {
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
					// Responses for abandoned (timed-out) requests are dropped.
				}
				Some(crate::TransportEvent::Request(incoming)) => {
					// `ping` answers immediately with an empty result; any
					// other server → client method gets method-not-found
					// (upstream's default handler).
					let response = if incoming.method == "ping" {
						JsonRpcResponse {
							jsonrpc: "2.0".into(),
							id: incoming.id,
							result: Some(serde_json::json!({})),
							error: None,
						}
					} else {
						JsonRpcResponse {
							jsonrpc: "2.0".into(),
							id: incoming.id,
							result: None,
							error: Some(JsonRpcError {
								code: METHOD_NOT_FOUND,
								message: format!("Method not found: {}", incoming.method),
								data: None,
							}),
						}
					};
					self.transport.send_response(&response).await?;
				}
				// Server notifications (progress, log, …) are skipped for now.
				Some(crate::TransportEvent::Notification(_)) => {}
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
	use std::time::Duration;
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

	/// Answer the `initialize` request with `protocol_version` (raw line out).
	async fn serve_initialize(
		to_server_rx: &mut mpsc::UnboundedReceiver<String>,
		to_client_tx: &mpsc::UnboundedSender<String>,
		protocol_version: &str,
	) {
		let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
		assert_eq!(message["method"], "initialize");
		// The request always proposes the client's latest protocol version.
		assert_eq!(
			message["params"]["protocolVersion"],
			crate::protocol::types::LATEST_PROTOCOL_VERSION
		);
		to_client_tx
			.send(
				json!({
					"jsonrpc": "2.0",
					"id": message["id"],
					"result": {
						"protocolVersion": protocol_version,
						"capabilities": {},
						"serverInfo": {"name": "fake", "version": "1.0"}
					}
				})
				.to_string(),
			)
			.unwrap();
	}

	#[tokio::test]
	async fn initialize_negotiates_every_supported_server_version() {
		for version in crate::protocol::types::SUPPORTED_PROTOCOL_VERSIONS {
			let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
			let expected = version.to_string();
			let server = tokio::spawn(async move {
				serve_initialize(&mut to_server_rx, &to_client_tx, &expected).await;
				// The handshake completes with `notifications/initialized`.
				let line = to_server_rx.recv().await.unwrap();
				let notification: Value = serde_json::from_str(&line).unwrap();
				assert_eq!(notification["method"], "notifications/initialized");
				assert!(notification.get("id").is_none());
			});

			let result = client.initialize("0.1.0").await.unwrap();
			server.await.unwrap();
			// The server's selection is accepted and kept for later probes.
			assert_eq!(result.protocol_version, version);
			assert_eq!(client.protocol_version(), Some(version));
		}
	}

	#[tokio::test]
	async fn initialize_rejects_unsupported_versions_and_closes_the_connection() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			serve_initialize(&mut to_server_rx, &to_client_tx, "1999-01-01").await;
			// The client closes the transport instead of finishing the
			// handshake — the in-memory close pushes a sentinel frame.
			let sentinel = to_server_rx.recv().await.unwrap();
			assert!(sentinel.contains("\"id\":null"), "{sentinel}");
		});

		let error = client.initialize("0.1.0").await.unwrap_err();
		server.await.unwrap();
		assert!(
			matches!(&error, crate::Error::Protocol(message)
				if message.contains("MCP server selected unsupported protocol version 1999-01-01")),
			"{error:?}"
		);
		// The closed transport refuses further work.
		assert!(client.ping().await.is_err());
		assert_eq!(client.protocol_version(), None);
	}

	#[tokio::test]
	async fn requests_time_out_and_notify_the_server_with_cancelled() {
		let (client, mut to_server_rx, _to_client_tx) = wired_client().await;
		// Keep `_to_client_tx` alive: dropping it would close the transport.
		let mut client = client.with_request_timeout_ms(Some(50));
		assert_eq!(client.request_timeout_ms(), 50);

		let error = client.call_tool("slow", Value::Null).await.unwrap_err();
		assert!(
			matches!(error, crate::Error::Timeout(ms) if ms == 50),
			"{error:?}"
		);

		// The abandoned request is cancelled on the wire: requestId + reason.
		let line = to_server_rx.recv().await.unwrap();
		let notification: Value = serde_json::from_str(&line).unwrap();
		assert_eq!(notification["method"], "tools/call");
		let line = to_server_rx.recv().await.unwrap();
		let notification: Value = serde_json::from_str(&line).unwrap();
		assert_eq!(notification["jsonrpc"], "2.0");
		assert_eq!(notification["method"], "notifications/cancelled");
		assert_eq!(notification["params"]["requestId"], 1);
		assert_eq!(notification["params"]["reason"], "Request timed out");
		assert!(notification.get("id").is_none());
	}

	#[tokio::test]
	async fn initialize_timeouts_do_not_send_cancelled() {
		let (client, mut to_server_rx, _to_client_tx) = wired_client().await;
		let mut client = client.with_request_timeout_ms(Some(50));

		let error = client.initialize("0.1.0").await.unwrap_err();
		assert!(matches!(error, crate::Error::Timeout(50)), "{error:?}");
		// The spec forbids cancelling `initialize`: the request frame went out,
		// but no `notifications/cancelled` follows it.
		let line = to_server_rx.recv().await.unwrap();
		let request: Value = serde_json::from_str(&line).unwrap();
		assert_eq!(request["method"], "initialize");
		assert!(to_server_rx.try_recv().is_err());
	}

	#[tokio::test]
	async fn a_disabled_timeout_waits_for_slow_servers() {
		let (client, mut to_server_rx, to_client_tx) = wired_client().await;
		let mut client = client.with_request_timeout_ms(Some(0));
		assert_eq!(client.request_timeout_ms(), 0);
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			tokio::time::sleep(Duration::from_millis(150)).await;
			to_client_tx
				.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {}}).to_string())
				.unwrap();
		});

		client.ping().await.unwrap();
		server.await.unwrap();
	}

	#[tokio::test]
	async fn ping_roundtrips_with_an_empty_result() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["method"], "ping");
			assert!(message.get("params").is_none());
			to_client_tx
				.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {}}).to_string())
				.unwrap();
		});

		client.ping().await.unwrap();
		server.await.unwrap();
	}

	#[tokio::test]
	async fn server_pings_are_answered_while_a_request_is_in_flight() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			assert_eq!(message["method"], "tools/call");
			// A server ping lands while the client waits for the tools/call
			// reply; it is answered immediately with an empty result.
			to_client_tx
				.send(json!({"jsonrpc": "2.0", "id": 99, "method": "ping"}).to_string())
				.unwrap();
			let line = to_server_rx.recv().await.unwrap();
			let answer: Value = serde_json::from_str(&line).unwrap();
			assert_eq!(answer["id"], 99);
			assert_eq!(answer["result"], json!({}));
			assert!(answer.get("error").is_none());
			// The original request still completes afterwards.
			to_client_tx
				.send(
					json!({"jsonrpc": "2.0", "id": message["id"], "result": {"content": [], "isError": false}})
						.to_string(),
				)
				.unwrap();
		});

		let result = client.call_tool("echo", Value::Null).await.unwrap();
		server.await.unwrap();
		assert!(result.content.is_empty());
	}

	#[tokio::test]
	async fn unknown_server_requests_get_method_not_found() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			to_client_tx
				.send(
					json!({"jsonrpc": "2.0", "id": 7, "method": "sampling/createMessage"})
						.to_string(),
				)
				.unwrap();
			let line = to_server_rx.recv().await.unwrap();
			let answer: Value = serde_json::from_str(&line).unwrap();
			assert_eq!(answer["id"], 7);
			assert_eq!(answer["error"]["code"], -32601);
			assert_eq!(
				answer["error"]["message"],
				"Method not found: sampling/createMessage"
			);
			to_client_tx
				.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {}}).to_string())
				.unwrap();
		});

		client.ping().await.unwrap();
		server.await.unwrap();
	}

	#[tokio::test]
	async fn tool_descriptors_pass_annotations_and_meta_through() {
		let (mut client, mut to_server_rx, to_client_tx) = wired_client().await;
		let server = tokio::spawn(async move {
			let message: Value = serde_json::from_str(&to_server_rx.recv().await.unwrap()).unwrap();
			to_client_tx
				.send(
					json!({"jsonrpc": "2.0", "id": message["id"], "result": {"tools": [{
						"name": "web", "description": "Fetch", "inputSchema": {"type": "object"},
						"title": "Web Fetch", "outputSchema": {"type": "object"},
						"annotations": {"title": "Fetch", "readOnlyHint": true,
							"destructiveHint": false, "idempotentHint": true, "openWorldHint": true},
						"execution": {"taskSupport": "forbidden"},
						"_meta": {"visibility": ["model"]}
					}]}})
					.to_string(),
				)
				.unwrap();
		});

		let tools = client.list_tools().await.unwrap();
		server.await.unwrap();
		let tool = &tools[0];
		assert_eq!(tool.title.as_deref(), Some("Web Fetch"));
		assert_eq!(tool.output_schema.as_ref().unwrap()["type"], "object");
		let annotations = tool.annotations.as_ref().unwrap();
		assert_eq!(annotations.title.as_deref(), Some("Fetch"));
		assert_eq!(annotations.read_only_hint, Some(true));
		assert_eq!(annotations.destructive_hint, Some(false));
		assert_eq!(annotations.idempotent_hint, Some(true));
		assert_eq!(annotations.open_world_hint, Some(true));
		assert_eq!(
			tool.execution.as_ref().unwrap().task_support.as_deref(),
			Some("forbidden")
		);
		assert_eq!(tool.meta.as_ref().unwrap()["visibility"], json!(["model"]));
	}
}
