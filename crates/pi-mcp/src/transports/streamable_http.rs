//! Streamable HTTP transport — port of
//! `pi/packages/mcp/src/transports/streamable-http.ts`.
//!
//! Framework stage: POST + SSE response stream with `Mcp-Session-Id` handling
//! is not wired yet.

use async_trait::async_trait;

use super::{Transport, TransportEvent};
use crate::protocol::jsonrpc::JsonRpcRequest;

pub struct StreamableHttpTransport {
	pub url: String,
	pub headers: Vec<(String, String)>,
}

impl StreamableHttpTransport {
	pub fn new(url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
		Self {
			url: url.into(),
			headers,
		}
	}
}

#[async_trait]
impl Transport for StreamableHttpTransport {
	async fn send(&mut self, _request: &JsonRpcRequest) -> crate::Result<()> {
		Err(crate::Error::Unsupported(
			"streamable-http transport not implemented yet (pi-rs framework stage)".into(),
		))
	}

	async fn recv(&mut self) -> crate::Result<Option<TransportEvent>> {
		Err(crate::Error::Unsupported(
			"streamable-http transport not implemented yet (pi-rs framework stage)".into(),
		))
	}

	async fn close(&mut self) -> crate::Result<()> {
		Ok(())
	}

	fn describe(&self) -> String {
		format!("streamable-http: {}", self.url)
	}

	fn supports_server_messages(&self) -> bool {
		true
	}
}
