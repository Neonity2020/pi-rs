use agent::{BoxFuture, Cancel, Tool, ToolResult};
use serde_json::{json, Value};

use crate::util::truncate_chars;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file's contents. Output is line-numbered as `N<TAB>text`. Use offset/limit for large files."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file, absolute or relative to the working directory."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start from."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default 2000)."
                }
            },
            "required": ["file_path"]
        })
    }

    fn run<'a>(&'a self, args: &'a Value, _cancel: &'a Cancel) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let Some(path) = args.get("file_path").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: file_path");
            };
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1) as usize;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LIMIT as u64) as usize;

            let bytes = match tokio::fs::read(path).await {
                Ok(bytes) => bytes,
                Err(error) => return ToolResult::err(format!("Failed to read {path}: {error}")),
            };
            let size = bytes.len();
            let Ok(text) = String::from_utf8(bytes) else {
                return ToolResult::err(format!("{path} is not UTF-8 text ({size} bytes)."));
            };

            let lines: Vec<&str> = text.lines().collect();
            let total = lines.len();
            if total == 0 {
                return ToolResult::ok(format!("0 lines. {path} is empty."));
            }

            let start = (offset - 1).min(total);
            let end = start.saturating_add(limit).min(total);
            let mut out = String::new();
            for (i, line) in lines[start..end].iter().enumerate() {
                out.push_str(&format!(
                    "{}\t{}\n",
                    start + i + 1,
                    truncate_chars(line, MAX_LINE_CHARS)
                ));
            }
            if end < total {
                out.push_str(&format!(
                    "… file has {total} lines; showing {}-{} (use offset/limit)\n",
                    start + 1,
                    end
                ));
            }
            ToolResult::ok(out.trim_end())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_file;

    fn args(value: Value) -> Value {
        value
    }

    #[tokio::test]
    async fn reads_numbered_lines() {
        let path = temp_file("read-all", "alpha\nbeta\ngamma\n");
        let result = ReadTool
            .run(&args(json!({"file_path": path})), &Cancel::new())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "1\talpha\n2\tbeta\n3\tgamma");
    }

    #[tokio::test]
    async fn honors_offset_and_limit() {
        let path = temp_file("read-slice", "a\nb\nc\nd\ne\n");
        let result = ReadTool
            .run(
                &args(json!({"file_path": path, "offset": 2, "limit": 2})),
                &Cancel::new(),
            )
            .await;
        assert_eq!(
            result.content,
            "2\tb\n3\tc\n… file has 5 lines; showing 2-3 (use offset/limit)"
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_error_result() {
        let result = ReadTool
            .run(
                &args(json!({"file_path": "/definitely/not/here"})),
                &Cancel::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("Failed to read"));
    }

    #[tokio::test]
    async fn binary_file_is_an_error_result() {
        let dir = crate::testutil::temp_dir("read-bin");
        let path = dir.join("blob.bin");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let result = ReadTool
            .run(&args(json!({"file_path": path})), &Cancel::new())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not UTF-8"));
    }
}
