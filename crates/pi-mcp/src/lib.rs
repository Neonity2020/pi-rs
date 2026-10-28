//! pi-mcp — Model Context Protocol client for pi-rs.
//!
//! Rust port of `pi/packages/mcp`: JSON-RPC protocol types, transports
//! (stdio / streamable-HTTP / in-memory), OAuth flow, and the client the MCP
//! extension uses to surface `mcp__<server>__<tool>` tools to the agent.

pub mod client;
pub mod protocol;
pub mod tool;
pub mod transports;

pub use client::{DEFAULT_REQUEST_TIMEOUT_MS, McpClient, McpServerConfig, McpToolInfo};
pub use protocol::jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
pub use protocol::types::{
	CallToolParams, CallToolResult, InitializeParams, InitializeResult, LATEST_PROTOCOL_VERSION,
	ListToolsResult, SUPPORTED_PROTOCOL_VERSIONS, ServerInfo, ToolAnnotations, ToolExecution,
};
pub use tool::{McpTool, SharedMcpClient, create_mcp_tool_name};
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
	#[error("request timed out after {0} ms")]
	Timeout(u64),
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	#[error("unsupported transport for this operation: {0}")]
	Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;
