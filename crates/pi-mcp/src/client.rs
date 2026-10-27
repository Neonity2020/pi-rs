//! MCP client — port of `pi/packages/mcp/src/client.ts`.
//!
//! Drives one server transport through initialize → tools/list → tools/call.
//! The coding-agent MCP extension registers each server's tools under the
//! `mcp__<server>__<tool>` namespace.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::protocol::types::{
	CallToolParams, CallToolResult, InitializeParams, InitializeResult, ListToolsResult, ServerInfo,
};
use crate::transports::Transport;

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
}

impl McpClient {
	pub fn new(server_name: impl Into<String>, transport: Box<dyn Transport>) -> Self {
		Self {
			server_name: server_name.into(),
			transport,
			next_id: 0,
			server_info: None,
		}
	}

	pub fn server_info(&self) -> Option<&InitializeResult> {
		self.server_info.as_ref()
	}

	/// initialize handshake, then `notifications/initialized`.
	pub async fn initialize(&mut self, client_version: &str) -> crate::Result<InitializeResult> {
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

	pub async fn list_tools(&mut self) -> crate::Result<Vec<McpToolInfo>> {
		let result: ListToolsResult = self.request("tools/list", serde_json::json!({})).await?;
		Ok(result
			.tools
			.into_iter()
			.map(|t| McpToolInfo {
				qualified_name: format!("mcp__{}__{}", self.server_name, t.name),
				server: self.server_name.clone(),
				name: t.name,
				description: t.description,
				input_schema: t.input_schema,
			})
			.collect())
	}

	pub async fn call_tool(&mut self, params: CallToolParams) -> crate::Result<CallToolResult> {
		self.request("tools/call", serde_json::to_value(&params)?)
			.await
	}

	async fn request<T: for<'de> Deserialize<'de>>(
		&mut self,
		method: &str,
		params: Value,
	) -> crate::Result<T> {
		self.next_id += 1;
		let id = self.next_id;
		let request = JsonRpcRequest::request(id, method, params);
		self.transport.send(&request).await?;

		loop {
			match self.transport.recv().await? {
				None => return Err(crate::Error::Closed),
				Some(crate::TransportEvent::Closed) => return Err(crate::Error::Closed),
				Some(crate::TransportEvent::Error(e)) => return Err(crate::Error::Transport(e)),
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
