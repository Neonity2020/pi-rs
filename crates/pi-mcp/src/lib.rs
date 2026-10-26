//! pi-mcp — Model Context Protocol client for pi-rs.
//!
//! Rust port of `pi/packages/mcp`: JSON-RPC protocol types, transports
//! (stdio / streamable-HTTP / in-memory), OAuth flow, and the client the MCP
//! extension uses to surface `mcp__<server>__<tool>` tools to the agent.

pub mod client;
pub mod protocol;
pub mod transports;

pub use client::{McpClient, McpServerConfig, McpToolInfo};
pub use protocol::jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use protocol::types::{
	CallToolParams, CallToolResult, InitializeParams, InitializeResult, ListToolsResult, ServerInfo,
};
pub use transports::{Transport, TransportEvent};

/// Crate error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("transport closed")]
	Closed,
	#[error("transport error: {0}")]
	Transport(String),
	#[error("protocol error: {0}")]
	Protocol(String),
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	#[error("unsupported transport for this operation: {0}")]
	Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;
