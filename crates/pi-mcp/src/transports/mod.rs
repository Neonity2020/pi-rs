//! MCP transports — port of `pi/packages/mcp/src/transports/*`.

pub mod in_memory;
pub mod stdio;
pub mod streamable_http;

use async_trait::async_trait;
use serde_json::Value;

use crate::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse};

/// Events surfaced by a transport to its client.
#[derive(Debug)]
pub enum TransportEvent {
	/// A JSON-RPC response to one of our requests.
	Message(Box<JsonRpcResponse>),
	/// A server → client request (a `method` carrying an `id`): the client is
	/// expected to answer, e.g. `ping` (upstream `handleRequest`).
	Request(Box<JsonRpcRequest>),
	/// A server → client notification (a `method` without an `id`), e.g.
	/// `notifications/progress`.
	Notification(Box<JsonRpcRequest>),
	/// Underlying connection closed.
	Closed,
	/// Fatal transport error.
	Error(String),
}

/// A byte/logical transport carrying JSON-RPC frames — the uniform contract
/// of `transport.ts` implementations.
#[async_trait]
pub trait Transport: Send {
	/// Perform the initialize handshake (where the transport needs one).
	async fn start(&mut self) -> crate::Result<()> {
		Ok(())
	}

	/// Write one request/notification frame.
	async fn send(&mut self, request: &JsonRpcRequest) -> crate::Result<()>;

	/// Write one response frame — answering a server → client request.
	async fn send_response(&mut self, response: &JsonRpcResponse) -> crate::Result<()>;

	/// Read the next inbound frame. `None` when the transport closed.
	async fn recv(&mut self) -> crate::Result<Option<TransportEvent>>;

	/// Close the transport and release resources.
	async fn close(&mut self) -> crate::Result<()>;

	/// Human-readable description for logs and `/mcp` status.
	fn describe(&self) -> String;

	/// True when the transport can deliver server-initiated messages.
	fn supports_server_messages(&self) -> bool {
		false
	}
}

/// Helper: parse a raw JSON line and classify it by JSON-RPC shape — a
/// `method` with an `id` is a server request, a `method` without one is a
/// notification, anything else is a response (mirrors upstream
/// `isJsonRpcRequest` / `isJsonRpcNotification` / `isJsonRpcResponse`).
pub fn parse_frame(line: &str) -> crate::Result<TransportEvent> {
	let value: Value = serde_json::from_str(line)
		.map_err(|e| crate::Error::Protocol(format!("invalid frame: {e}")))?;
	let has_method = value.get("method").is_some_and(Value::is_string);
	let has_id = value.get("id").is_some_and(|id| !id.is_null());
	if has_method && has_id {
		let request: JsonRpcRequest = serde_json::from_value(value)?;
		Ok(TransportEvent::Request(Box::new(request)))
	} else if has_method {
		let notification: JsonRpcRequest = serde_json::from_value(value)?;
		Ok(TransportEvent::Notification(Box::new(notification)))
	} else {
		let frame: JsonRpcResponse = serde_json::from_value(value)?;
		Ok(TransportEvent::Message(Box::new(frame)))
	}
}

/// Helper: encode a frame for line-based transports.
pub fn encode_frame(request: &JsonRpcRequest) -> String {
	serde_json::to_string(request).unwrap_or_else(|_| "{}".to_string())
}

/// Helper: encode a response frame for line-based transports.
pub fn encode_response(response: &JsonRpcResponse) -> String {
	serde_json::to_string(response).unwrap_or_else(|_| "{}".to_string())
}
