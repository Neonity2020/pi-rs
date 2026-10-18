use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use agent::{BoxFuture, Cancel, Tool, ToolResult};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::MAX_OUTPUT_BYTES;
use crate::util::{truncate_bytes, truncate_lines};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;
const CAPTURE_BYTES: usize = 16 * 1024;

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory. Returns bounded stdout/stderr head/tail previews (at most 64 KiB total). Optional `timeout` in seconds, default 120."
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
            cmd.arg("-lc").arg(command);
            run_process(cmd, timeout_secs, cancel).await
        })
    }
}

/// Shared process lifecycle; tests can use a non-login shell without
/// loading the developer's shell initialization files.
async fn run_process(
    mut cmd: tokio::process::Command,
    timeout_secs: u64,
    cancel: &Cancel,
) -> ToolResult {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let shell = cmd.as_std().get_program().to_string_lossy().into_owned();
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
            let (out_read, err_read) = tokio::join!(capture(&mut stdout), capture(&mut stderr),);
            let stdout = out_read?;
            let stderr = err_read?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((stdout, stderr, status))
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
}

/// Always drain the pipe, retaining only a fixed head/tail. Stopping reads
/// at the budget would block a child that is still writing to a full pipe.
async fn capture(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<String> {
    let half = CAPTURE_BYTES / 2;
    let mut head = Vec::with_capacity(half);
    let mut tail = Vec::with_capacity(CAPTURE_BYTES);
    let mut total = 0u64;
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        let kept = (half - head.len()).min(n);
        head.extend_from_slice(&buf[..kept]);
        tail.extend_from_slice(&buf[kept..n]);
        if tail.len() > half {
            tail.drain(..tail.len() - half);
        }
    }
    let omitted = total.saturating_sub((head.len() + tail.len()) as u64);
    if omitted == 0 {
        head.extend_from_slice(&tail);
        return Ok(String::from_utf8_lossy(&head).into_owned());
    }
    // Do not turn a valid character cut at the capture boundary into U+FFFD.
    let head = match std::str::from_utf8(&head) {
        Err(error) if error.error_len().is_none() => &head[..error.valid_up_to()],
        _ => head.as_slice(),
    };
    let skip = tail
        .iter()
        .take(3)
        .take_while(|b| **b & 0xc0 == 0x80)
        .count();
    let tail = &tail[skip..];
    let omitted = total - (head.len() + tail.len()) as u64;
    let mut out = String::from_utf8_lossy(head).into_owned();
    out.push_str(&format!("\n… [{omitted} bytes omitted] …\n"));
    out.push_str(&String::from_utf8_lossy(tail));
    Ok(out)
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
    let text = truncate_bytes(&text, MAX_OUTPUT_BYTES);
    if success {
        ToolResult::ok(text)
    } else {
        ToolResult::err(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_command(script: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[tokio::test]
    async fn utf8_output_below_the_budget_is_preserved_verbatim() {
        let text = "中".repeat(3000);
        assert_eq!(capture(&mut text.as_bytes()).await.unwrap(), text);
    }

    #[tokio::test]
    async fn clipped_utf8_output_keeps_valid_head_and_tail() {
        let text = "中".repeat(10000);
        let out = capture(&mut text.as_bytes()).await.unwrap();
        assert!(out.starts_with('中') && out.ends_with('中'));
        assert!(out.contains("bytes omitted"));
        assert!(!out.contains('�'));
        assert!(out.len() < 17 * 1024);
    }

    #[tokio::test]
    async fn runs_simple_commands() {
        let result = run_process(test_command("echo hello"), 120, &Cancel::new()).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content.trim(), "hello");
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_result_with_code() {
        let result = run_process(test_command("echo fail >&2; exit 3"), 120, &Cancel::new()).await;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn huge_stdout_and_stderr_are_bounded_and_fully_drained() {
        let result = tokio::time::timeout(Duration::from_secs(30), run_process(
            test_command("printf STDOUT_START; head -c 2097152 /dev/zero | tr '\\000' x; printf '\\nSTDOUT_END\\n'; printf STDERR_START >&2; head -c 2097152 /dev/zero | tr '\\000' y >&2; printf '\\nSTDERR_END\\n' >&2"),
            120,
            &Cancel::new(),
        )).await.expect("pipes must be drained even after the capture budget is reached");
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.len() <= 64 * 1024,
            "retained {} bytes",
            result.content.len()
        );
        for marker in [
            "STDOUT_START",
            "STDOUT_END",
            "STDERR_START",
            "STDERR_END",
            "omitted",
        ] {
            assert!(result.content.contains(marker), "missing {marker}");
        }
    }

    #[tokio::test]
    async fn cancellation_kills_the_command() {
        let cancel = Cancel::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { run_process(test_command("sleep 30"), 120, &cancel).await }
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
            let release = dir.join("release");
            let leader = dir.join("leader-pid");
            let grandchild = dir.join("grandchild.sh");
            let child = dir.join("child.sh");
            std::fs::write(
                &grandchild,
                format!(
                    "printf ready > '{}'; while [ ! -e '{}' ]; do sleep 0.02; done; printf survived > '{}'",
                    ready.display(),
                    release.display(),
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
                    run_process(
                        test_command(&command),
                        if mode == "timeout" { 3 } else { 30 },
                        &cancel,
                    )
                    .await
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !ready.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{mode}: grandchild should start"));
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
            std::fs::write(&release, "go").unwrap();
            tokio::time::sleep(Duration::from_millis(2200)).await;
            assert!(
                !marker.exists(),
                "{mode}: a descendant wrote after the tool stopped"
            );
        }
    }
}
