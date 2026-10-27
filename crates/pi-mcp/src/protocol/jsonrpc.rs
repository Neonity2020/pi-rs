//! JSON-RPC 2.0 envelope — port of `pi/packages/mcp/src/protocol/jsonrpc.ts`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request/notification frame written to a transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
	pub jsonrpc: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub id: Option<Value>,
	pub method: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub params: Option<Value>,
}

impl JsonRpcRequest {
	pub fn request(id: u64, method: impl Into<String>, params: Value) -> Self {
		Self {
			jsonrpc: "2.0".into(),
			id: Some(Value::from(id)),
			method: method.into(),
			params: Some(params),
		}
	}

	pub fn notification(method: impl Into<String>, params: Value) -> Self {
		Self {
			jsonrpc: "2.0".into(),
			id: None,
			method: method.into(),
			params: Some(params),
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
	pub code: i64,
	pub message: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data: Option<Value>,
}

/// JSON-RPC response frame read from a transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
	pub jsonrpc: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub id: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub result: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
	pub fn is_error(&self) -> bool {
		self.error.is_some()
	}
}
