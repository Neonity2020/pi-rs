use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// What a tool produced. Tool failures are results, not errors: they go back
/// to the model as text and never abort the loop (Pi philosophy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// The tool boundary. Four instances of this cover all coding work
/// (read / write / edit / bash); anything else is a wrapper around bash.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the tool arguments.
    fn parameters(&self) -> serde_json::Value;

    fn run<'a>(
        &'a self,
        args: &'a serde_json::Value,
        cancel: &'a crate::Cancel,
    ) -> BoxFuture<'a, ToolResult>;
}
