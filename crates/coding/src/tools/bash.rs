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
            #[cfg(unix)]
            cmd.process_group(0);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(error) => return ToolResult::err(format!("Failed to spawn {shell}: {error}")),
            };
            #[cfg(unix)]
            let mut group = ProcessGroup(child.id().map(|id| id as libc::pid_t));
            let mut stdout = child.stdout.take().expect("piped stdout");
            let mut stderr = child.stderr.take().expect("piped stderr");

            let result = {
                let collect = async {
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
                    _ = cancel.cancelled() => Err(ToolResult::err("Cancelled.")),
                    result = tokio::time::timeout(Duration::from_secs(timeout_secs), collect) => {
                        match result {
                            Err(_elapsed) => Err(ToolResult::err(format!(
                                "Timed out after {timeout_secs}s."
                            ))),
                            Ok(Err(error)) => Err(ToolResult::err(format!("Failed to run command: {error}"))),
                            Ok(Ok(output)) => Ok(output),
                        }
                    }
                }
            };

            match result {
                Ok((stdout, stderr, status)) => {
                    #[cfg(unix)]
                    {
                        group.0 = None;
                    }
                    format_result(stdout, stderr, status)
                }
                Err(mut result) => {
                    #[cfg(unix)]
                    if let Err(error) = group.terminate() {
                        result
                            .content
                            .push_str(&format!(" Failed to kill process group: {error}."));
                    }
                    // Reap the direct child before reporting cancellation or
                    // timeout. The group guard also covers a dropped future.
                    let _ = child.start_kill();
                    if let Err(error) = child.wait().await {
                        result
                            .content
                            .push_str(&format!(" Failed to reap command: {error}."));
                    }
                    result
                }
            }
        })
    }
}

#[cfg(unix)]
struct ProcessGroup(Option<libc::pid_t>);

#[cfg(unix)]
impl ProcessGroup {
    fn terminate(&mut self) -> std::io::Result<()> {
        let Some(id) = self.0.take() else {
            return Ok(());
        };
        // SAFETY: process_group(0) made the spawned child its group leader.
        // Its positive PID is owned here; killpg never targets our own group.
        if unsafe { libc::killpg(id, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        self.0 = Some(id);
        Err(error)
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = self.terminate();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_timeout_and_dropped_future_stop_descendants() {
        for mode in ["cancel", "timeout", "drop"] {
            let dir = crate::testutil::temp_dir(mode);
            let ready = dir.join("ready");
            let marker = dir.join("survived");
            let leader = dir.join("leader-pid");
            let grandchild = dir.join("grandchild.sh");
            let child = dir.join("child.sh");
            std::fs::write(
                &grandchild,
                format!(
                    "printf ready > '{}'; sleep 2; printf survived > '{}'",
                    ready.display(),
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::write(&child, format!("sh '{}' & wait", grandchild.display())).unwrap();
            let cancel = Cancel::new();
            let handle = tokio::spawn({
                let cancel = cancel.clone();
                let command = format!(
                    "printf '%s' $$ > '{}'; sh '{}' & wait",
                    leader.display(),
                    child.display()
                );
                async move {
                    BashTool.run(&json!({"command": command, "timeout": if mode == "timeout" { 1 } else { 30 }}), &cancel).await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !ready.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("grandchild should start");
            match mode {
                "cancel" => cancel.cancel(),
                "drop" => handle.abort(),
                _ => {}
            }
            let result = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap();
            if mode == "drop" {
                assert!(result.unwrap_err().is_cancelled());
            } else {
                assert!(result.unwrap().is_error);
            }
            let pid: libc::pid_t = std::fs::read_to_string(&leader).unwrap().parse().unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    // SAFETY: signal 0 only probes this test's child PID.
                    if unsafe { libc::kill(pid, 0) } == -1 {
                        assert_eq!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::ESRCH)
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the direct child must be reaped");
            tokio::time::sleep(Duration::from_millis(2200)).await;
            assert!(
                !marker.exists(),
                "{mode}: a descendant wrote after the tool stopped"
            );
        }
    }
}
