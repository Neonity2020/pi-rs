//! stdio transport — port of `pi/packages/mcp/src/transports/stdio.ts`.
//!
//! Framework stage: the frame loop (newline-delimited JSON over the child's
//! stdin/stdout, stderr forwarded to diagnostics) is not wired yet.

use async_trait::async_trait;

use super::{Transport, TransportEvent};
use crate::protocol::jsonrpc::JsonRpcRequest;

pub struct StdioTransport {
	pub command: String,
	pub args: Vec<String>,
	pub env: Vec<(String, String)>,
}

impl StdioTransport {
	pub fn new(command: impl Into<String>, args: Vec<String>, env: Vec<(String, String)>) -> Self {
		Self {
			command: command.into(),
			args,
			env,
		}
	}
}

#[async_trait]
impl Transport for StdioTransport {
	async fn send(&mut self, _request: &JsonRpcRequest) -> crate::Result<()> {
		Err(crate::Error::Unsupported(
			"stdio transport not implemented yet (pi-rs framework stage)".into(),
		))
	}

	async fn recv(&mut self) -> crate::Result<Option<TransportEvent>> {
		Err(crate::Error::Unsupported(
			"stdio transport not implemented yet (pi-rs framework stage)".into(),
		))
	}

	async fn close(&mut self) -> crate::Result<()> {
		Ok(())
	}

	fn describe(&self) -> String {
		format!("stdio: {} {}", self.command, self.args.join(" "))
	}
}
