//! MCP content blocks — port of `pi/packages/mcp/src/protocol/content.ts`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
	Text { text: String },
	Image { data: String, mime_type: String },
	Resource { resource: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceContents {
	pub uri: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub mime_type: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub text: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub blob: Option<String>,
}
