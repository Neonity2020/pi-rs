//! MCP transports — port of `pi/packages/mcp/src/transports/*`.

pub mod in_memory;
pub mod stdio;
pub mod streamable_http;

use async_trait::async_trait;

use crate::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse};

/// Events surfaced by a transport to its client.
#[derive(Debug)]
pub enum TransportEvent {
	Message(Box<JsonRpcResponse>),
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

/// Helper: parse a raw JSON line into a response frame.
pub fn parse_frame(line: &str) -> crate::Result<JsonRpcResponse> {
	serde_json::from_str(line).map_err(|e| crate::Error::Protocol(format!("invalid frame: {e}")))
}

/// Helper: encode a frame for line-based transports.
pub fn encode_frame(request: &JsonRpcRequest) -> String {
	serde_json::to_string(request).unwrap_or_else(|_| "{}".to_string())
}
