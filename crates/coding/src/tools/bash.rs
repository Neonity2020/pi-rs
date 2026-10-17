use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use agent::{BoxFuture, Cancel, Tool, ToolResult};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::util::truncate_lines;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory. Returns stdout/stderr (truncated if long). Optional `timeout` in seconds, default 120."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 120, max 3600)."
                }
            },
            "required": ["command"]
        })
    }

    fn run<'a>(&'a self, args: &'a Value, cancel: &'a Cancel) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return ToolResult::err("Missing required argument: command");
            };
            let timeout_secs = args
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS);

            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            let mut cmd = tokio::process::Command::new(&shell);
            cmd.arg("-lc")
                .arg(command)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(error) => return ToolResult::err(format!("Failed to spawn {shell}: {error}")),
            };
            let mut stdout = child.stdout.take().expect("piped stdout");
            let mut stderr = child.stderr.take().expect("piped stderr");

            let collect = async move {
                let mut out_bytes = Vec::new();
                let mut err_bytes = Vec::new();
                let (out_read, err_read) = tokio::join!(
                    stdout.read_to_end(&mut out_bytes),
                    stderr.read_to_end(&mut err_bytes),
                );
                out_read?;
                err_read?;
                let status = child.wait().await?;
                Ok::<_, std::io::Error>((
                    String::from_utf8_lossy(&out_bytes).into_owned(),
                    String::from_utf8_lossy(&err_bytes).into_owned(),
                    status,
                ))
            };

            tokio::select! {
                biased;
                _ = cancel.cancelled() => ToolResult::err("Cancelled."),
                result = tokio::time::timeout(Duration::from_secs(timeout_secs), collect) => {
                    match result {
                        Err(_elapsed) => ToolResult::err(format!(
                            "Timed out after {timeout_secs}s and killed the command."
                        )),
                        Ok(Err(error)) => ToolResult::err(format!("Failed to run command: {error}")),
                        Ok(Ok((stdout, stderr, status))) => format_result(stdout, stderr, status),
                    }
                }
            }
        })
    }
}

fn format_result(stdout: String, stderr: String, status: ExitStatus) -> ToolResult {
    let mut combined = stdout;
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    let mut text = truncate_lines(combined.trim_end(), 300, 100);
    let success = status.success();
    if !success {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "killed by signal".to_string());
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[exit code: {code}]"));
    }
    if text.is_empty() {
        text = "OK (no output)".to_string();
    }
    if success {
        ToolResult::ok(text)
    } else {
        ToolResult::err(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_simple_commands() {
        let result = BashTool
            .run(&json!({"command": "echo hello"}), &Cancel::new())
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content.trim(), "hello");
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_result_with_code() {
        let result = BashTool
            .run(&json!({"command": "echo fail >&2; exit 3"}), &Cancel::new())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("fail"));
        assert!(result.content.contains("[exit code: 3]"));
    }

    #[tokio::test]
    async fn missing_command_is_an_error_result() {
        let result = BashTool.run(&json!({}), &Cancel::new()).await;
        assert!(result.is_error);
        assert!(result.content.contains("command"));
    }

    #[tokio::test]
    async fn cancellation_kills_the_command() {
        let cancel = Cancel::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { BashTool.run(&json!({"command": "sleep 30"}), &cancel).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("cancelled command should return promptly")
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Cancelled"));
    }
}
