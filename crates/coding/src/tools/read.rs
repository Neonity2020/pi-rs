use agent::{BoxFuture, Cancel, Tool, ToolResult};
use serde_json::{json, Value};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use super::MAX_OUTPUT_BYTES;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file's contents. Output is line-numbered as `N<TAB>text`, capped at 64 KiB with long lines truncated. Use offset/limit or the continuation offset for large files."
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

    fn run<'a>(&'a self, args: &'a Value, cancel: &'a Cancel) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let Some(path) = args.get("file_path").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: file_path");
            };
            let offset = args
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .max(1);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_LIMIT as u64);
            if limit == 0 {
                return ToolResult::err("limit must be greater than zero");
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => ToolResult::err("Cancelled."),
                result = read_range(path, offset, limit) => match result {
                    Ok(result) => result,
                    Err(error) => ToolResult::err(format!("Failed to read {path}: {error}")),
                }
            }
        })
    }
}

/// Keep only the displayed prefix of each line. `fill_buf`/`consume` also
/// discard huge skipped lines without ever allocating a complete line.
async fn line_prefix(
    reader: &mut (impl AsyncBufRead + Unpin),
    keep: bool,
) -> std::io::Result<Option<String>> {
    const PREFIX_BYTES: usize = MAX_LINE_CHARS * 4 + 1;
    let mut prefix = Vec::new();
    let mut saw_bytes = false;
    let mut clipped = false;
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            break;
        }
        saw_bytes = true;
        let newline = bytes.iter().position(|b| *b == b'\n');
        let n = newline.unwrap_or(bytes.len());
        if keep {
            let kept = n.min(PREFIX_BYTES - prefix.len());
            prefix.extend_from_slice(&bytes[..kept]);
            clipped |= kept < n;
        }
        reader.consume(n + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    if !saw_bytes {
        return Ok(None);
    }
    if !keep {
        return Ok(Some(String::new()));
    }
    if !clipped && prefix.last() == Some(&b'\r') {
        prefix.pop();
    }
    let text = match std::str::from_utf8(&prefix) {
        Ok(text) => text,
        Err(error) if clipped && error.error_len().is_none() => {
            std::str::from_utf8(&prefix[..error.valid_up_to()]).unwrap()
        }
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not UTF-8 text",
            ))
        }
    };
    let mut out: String = text.chars().take(MAX_LINE_CHARS).collect();
    if clipped || text.chars().count() > MAX_LINE_CHARS {
        out.push('…');
    }
    Ok(Some(out))
}

async fn read_range(path: &str, offset: u64, limit: u64) -> std::io::Result<ToolResult> {
    let mut reader = BufReader::new(tokio::fs::File::open(path).await?);
    let mut number = 1u64;
    let mut shown = 0u64;
    let mut out = String::new();
    while shown < limit {
        let keep = number >= offset;
        let Some(line) = line_prefix(&mut reader, keep).await? else {
            break;
        };
        if keep {
            let formatted = format!("{number}\t{line}\n");
            // Leave room for the continuation hint and its line number.
            if out.len() + formatted.len() + 128 > MAX_OUTPUT_BYTES {
                out.push_str(&format!(
                    "… output limit reached; continue with offset {number}\n"
                ));
                return Ok(ToolResult::ok(out.trim_end()));
            }
            out.push_str(&formatted);
            shown += 1;
        }
        number = number.saturating_add(1);
    }
    if !reader.fill_buf().await?.is_empty() {
        out.push_str(&format!(
            "… more lines; showing {offset}-{} (use offset {number}/limit)\n",
            number - 1
        ));
    }
    if number == 1 {
        return Ok(ToolResult::ok(format!("0 lines. {path} is empty.")));
    }
    if out.is_empty() {
        return Ok(ToolResult::ok(format!("No lines at offset {offset}.")));
    }
    Ok(ToolResult::ok(out.trim_end()))
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
            "2\tb\n3\tc\n… more lines; showing 2-3 (use offset 4/limit)"
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

    #[tokio::test]
    async fn range_reads_do_not_parse_the_rest_of_the_file() {
        let path = crate::testutil::temp_dir("read-range").join("file");
        std::fs::write(&path, b"first\n\xff\xff\xff").unwrap();
        let result = ReadTool
            .run(&json!({"file_path": path, "limit": 1}), &Cancel::new())
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.starts_with("1\tfirst"));
        assert!(result.content.contains("offset"));
    }

    #[tokio::test]
    async fn huge_requested_ranges_have_a_bounded_result_and_resume_offset() {
        let path = temp_file("read-budget", &("中".repeat(2000) + "\n").repeat(40));
        let result = ReadTool
            .run(
                &json!({"file_path": path, "limit": u64::MAX}),
                &Cancel::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.len() <= 64 * 1024,
            "retained {} bytes",
            result.content.len()
        );
        assert!(result.content.contains("output limit"));
        assert!(result.content.contains("offset 11"));
    }

    #[tokio::test]
    async fn a_huge_single_line_is_truncated_without_losing_the_next_line() {
        let path = temp_file(
            "read-huge-line",
            &("x".repeat(4 * 1024 * 1024) + "\nlast\n"),
        );
        let result = ReadTool
            .run(&json!({"file_path": path}), &Cancel::new())
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.starts_with("1\t"));
        assert!(result.content.contains("…\n2\tlast"));
        assert!(result.content.len() < 8100);
        assert!(!result.content.contains('�'));
    }
}
