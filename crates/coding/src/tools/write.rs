use agent::{BoxFuture, Cancel, Tool, ToolResult};
use serde_json::{json, Value};

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write a file, creating it (and parent directories) or overwriting it entirely."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, absolute or relative to the working directory."
                },
                "content": {
                    "type": "string",
                    "description": "Full contents of the file."
                }
            },
            "required": ["file_path", "content"]
        })
    }

    fn run<'a>(&'a self, args: &'a Value, cancel: &'a Cancel) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let Some(path) = args.get("file_path").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: file_path");
            };
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: content");
            };
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(error) = tokio::fs::create_dir_all(parent).await {
                        return ToolResult::err(format!(
                            "Failed to create {}: {error}",
                            parent.display()
                        ));
                    }
                }
            }
            match super::atomic::write(std::path::Path::new(path), content.as_bytes(), cancel) {
                Ok(()) => {
                    let lines = content.lines().count();
                    ToolResult::ok(format!("Wrote {lines} lines to {path}."))
                }
                Err(error) => ToolResult::err(format!("Failed to write {path}: {error}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_and_creates_parents() {
        let dir = crate::testutil::temp_dir("write");
        let path = dir.join("nested/deep/file.txt");
        let result = WriteTool
            .run(
                &json!({"file_path": path.to_str().unwrap(), "content": "hello\nworld\n"}),
                &Cancel::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
        assert!(result.content.contains("Wrote 2 lines"));
    }

    #[tokio::test]
    async fn missing_content_is_an_error_result() {
        let result = WriteTool
            .run(&json!({"file_path": "/tmp/x"}), &Cancel::new())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("content"));
    }
}
