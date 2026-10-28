//! MCP method payload types — port of `pi/packages/mcp/src/protocol/types.ts`.
//! Framework stage covers the initialize/list-tools/call-tool surface the
//! coding agent's MCP extension needs; resources, prompts, sampling, and
//! elicitation land with the full client.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::Content;

/// The protocol version this client requests in `initialize`.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// Versions the client accepts from a server. Servers that do not support the
/// requested version answer with their own latest one, so older versions stay
/// accepted for servers built on older SDKs.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [
	LATEST_PROTOCOL_VERSION,
	"2025-06-18",
	"2025-03-26",
	"2024-11-05",
];

/// True when a server-selected `protocolVersion` may be accepted.
pub fn is_supported_protocol_version(version: &str) -> bool {
	SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
	pub protocol_version: String,
	pub capabilities: Value,
	pub server_info: ServerInfo,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub instructions: Option<String>,
}

/// Hints about how a tool behaves — passthrough modeling of upstream
/// `ToolAnnotations`; the tool bridge does not act on them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub read_only_hint: Option<bool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub destructive_hint: Option<bool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub idempotent_hint: Option<bool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub open_world_hint: Option<bool>,
}

/// Execution preferences for a tool (upstream `ToolExecution`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecution {
	/// `forbidden` | `optional` | `required`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub task_support: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	pub description: Option<String>,
	#[serde(default)]
	pub input_schema: Value,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub output_schema: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub annotations: Option<ToolAnnotations>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub execution: Option<ToolExecution>,
	#[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
	pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
	#[serde(default)]
	pub content: Vec<Content>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub structured_content: Option<Value>,
	#[serde(default)]
	pub is_error: bool,
}
