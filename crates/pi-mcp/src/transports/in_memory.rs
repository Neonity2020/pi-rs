//! In-memory transport — port of `pi/packages/mcp/src/transports/in-memory.ts`.
//! Used by tests and by embedded MCP servers living in the same process.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{Transport, TransportEvent};
use crate::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse};

/// One end of a connected in-memory pair carrying newline-free JSON frames.
pub struct InMemoryTransport {
	name: String,
	inbound: mpsc::UnboundedReceiver<String>,
	outbound: mpsc::UnboundedSender<String>,
	closed: bool,
}

/// Create a connected pair `(a, b)`; frames sent by `a` are received by `b`
/// and vice versa.
pub fn pair() -> (InMemoryTransport, InMemoryTransport) {
	let (a_out, b_in) = mpsc::unbounded_channel::<String>();
	let (b_out, a_in) = mpsc::unbounded_channel::<String>();
	(
		InMemoryTransport::new("in-memory(a)", a_in, a_out),
		InMemoryTransport::new("in-memory(b)", b_in, b_out),
	)
}

impl InMemoryTransport {
	fn new(
		name: &str,
		inbound: mpsc::UnboundedReceiver<String>,
		outbound: mpsc::UnboundedSender<String>,
	) -> Self {
		Self {
			name: name.into(),
			inbound,
			outbound,
			closed: false,
		}
	}
}

/// Build a transport over existing raw line channels — the seam for embedded
/// servers and tests whose peer speaks JSON lines (and needs the request
/// `method`, which the parsed [`crate::transports::TransportEvent`] drops).
pub fn from_raw(
	name: &str,
	inbound: mpsc::UnboundedReceiver<String>,
	outbound: mpsc::UnboundedSender<String>,
) -> InMemoryTransport {
	InMemoryTransport::new(name, inbound, outbound)
}

#[async_trait]
impl Transport for InMemoryTransport {
	async fn send(&mut self, request: &JsonRpcRequest) -> crate::Result<()> {
		self.outbound
			.send(super::encode_frame(request))
			.map_err(|_| crate::Error::Closed)
	}

	async fn send_response(&mut self, response: &JsonRpcResponse) -> crate::Result<()> {
		self.outbound
			.send(super::encode_response(response))
			.map_err(|_| crate::Error::Closed)
	}

	async fn recv(&mut self) -> crate::Result<Option<TransportEvent>> {
		if self.closed {
			return Ok(None);
		}
		match self.inbound.recv().await {
			Some(line) => match super::parse_frame(&line) {
				Ok(event) => Ok(Some(event)),
				Err(e) => Ok(Some(TransportEvent::Error(e.to_string()))),
			},
			None => {
				self.closed = true;
				Ok(None)
			}
		}
	}

	async fn close(&mut self) -> crate::Result<()> {
		self.closed = true;
		self.outbound
			.send("{\"jsonrpc\":\"2.0\",\"id\":null,\"result\":null}".into())
			.ok();
		Ok(())
	}

	fn describe(&self) -> String {
		self.name.clone()
	}

	fn supports_server_messages(&self) -> bool {
		true
	}
}
