use agent::{BoxFuture, Cancel, Tool, ToolResult};
use serde_json::{json, Value};

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file. `old_string` must appear exactly once unless replace_all is true. Read the file first and copy the text exactly."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, absolute or relative to the working directory."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default false: exactly one required)."
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn run<'a>(&'a self, args: &'a Value, _cancel: &'a Cancel) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let Some(path) = args.get("file_path").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: file_path");
            };
            let Some(old_string) = args.get("old_string").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: old_string");
            };
            let Some(new_string) = args.get("new_string").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: new_string");
            };
            let replace_all = args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if old_string.is_empty() {
                return ToolResult::err("old_string must not be empty.");
            }

            let text = match tokio::fs::read_to_string(path).await {
                Ok(text) => text,
                Err(error) => return ToolResult::err(format!("Failed to read {path}: {error}")),
            };

            let occurrences = text.matches(old_string).count();
            if occurrences == 0 {
                return ToolResult::err(format!(
                    "old_string not found in {path}. Read the file and copy the exact text (including whitespace)."
                ));
            }
            if occurrences > 1 && !replace_all {
                return ToolResult::err(format!(
                    "old_string appears {occurrences} times in {path}. Add more surrounding context to make it unique, or set replace_all=true."
                ));
            }

            let replaced = if replace_all { occurrences } else { 1 };
            let new_text = if replace_all {
                text.replace(old_string, new_string)
            } else {
                text.replacen(old_string, new_string, 1)
            };
            match tokio::fs::write(path, new_text).await {
                Ok(()) => ToolResult::ok(format!(
                    "Edited {path}: replaced {replaced} occurrence{}.",
                    if replaced == 1 { "" } else { "s" }
                )),
                Err(error) => ToolResult::err(format!("Failed to write {path}: {error}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_file;

    #[tokio::test]
    async fn replaces_a_unique_string() {
        let path = temp_file("edit-once", "fn main() {\n    old();\n}\n");
        let result = EditTool
            .run(
                &json!({"file_path": path, "old_string": "    old();", "new_string": "    new();"}),
                &Cancel::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("replaced 1 occurrence"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("new()"));
    }

    #[tokio::test]
    async fn ambiguous_match_is_rejected() {
        let path = temp_file("edit-many", "x\nx\n");
        let result = EditTool
            .run(
                &json!({"file_path": path, "old_string": "x", "new_string": "y"}),
                &Cancel::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("appears 2 times"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\nx\n");
    }

    #[tokio::test]
    async fn replace_all_replaces_everything() {
        let path = temp_file("edit-all", "x\nx\nx\n");
        let result = EditTool
            .run(
                &json!({"file_path": path, "old_string": "x", "new_string": "y", "replace_all": true}),
                &Cancel::new(),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("3 occurrences"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\ny\ny\n");
    }

    #[tokio::test]
    async fn not_found_is_an_error_result() {
        let path = temp_file("edit-none", "hello\n");
        let result = EditTool
            .run(
                &json!({"file_path": path, "old_string": "nope", "new_string": "y"}),
                &Cancel::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }
}
