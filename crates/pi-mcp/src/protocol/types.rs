//! MCP method payload types — port of `pi/packages/mcp/src/protocol/types.ts`.
//! Framework stage covers the initialize/list-tools/call-tool surface the
//! coding agent's MCP extension needs; resources, prompts, sampling, and
//! elicitation land with the full client.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::Content;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
	#[serde(default)]
	pub protocol_version: String,
	pub capabilities: Value,
	pub client_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
	pub name: String,
	pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
	pub protocol_version: String,
	pub capabilities: Value,
	pub server_info: ServerInfo,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
	pub name: String,
	pub description: Option<String>,
	#[serde(default)]
	pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
	pub tools: Vec<ToolInfo>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
	pub name: String,
	#[serde(default)]
	pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
	#[serde(default)]
	pub content: Vec<Content>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub structured_content: Option<Value>,
	#[serde(default)]
	pub is_error: bool,
}
