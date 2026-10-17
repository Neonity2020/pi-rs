//! End-to-end tests: run the real `pi` binary against a local mock of the
//! DeepSeek chat completions API (SSE).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// A minimal HTTP/1.1 server that answers each request with one canned SSE
/// body and records each request body.
fn start_mock_api(sse_bodies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();

    std::thread::spawn(move || {
        for body in sse_bodies {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            if let Some(json) = request {
                recorded.lock().unwrap().push(json);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });

    (format!("http://{addr}"), requests)
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Value> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        let Some(header_end) = find(&buffer, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
        let length: usize = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        let body_start = header_end + 4;
        if buffer.len() >= body_start + length {
            return serde_json::from_slice(&buffer[body_start..body_start + length]).ok();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn sse(events: &[&str]) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(event);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn pi() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    // Isolate sessions and keys from the developer's environment.
    command.env("PI_RS_HOME", test_home());
    command.env_remove("DEEPSEEK_API_KEY");
    command.env_remove("OPENAI_API_KEY");
    command
}

fn test_home() -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "pi-rs-e2e-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

#[test]
fn print_mode_streams_text() {
    let (base_url, requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#,
        r#"{"choices":[{"delta":{"content":"Hello "}}]}"#,
        r#"{"choices":[{"delta":{"content":"world"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2,"prompt_cache_hit_tokens":4}}"#,
    ])]);

    let output = pi()
        .args([
            "-p",
            "say hi",
            "--base-url",
            &base_url,
            "--api-key",
            "test-key",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Hello world"), "stdout: {stdout}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body = &requests[0];
    assert_eq!(body["model"], "deepseek-flash");
    assert_eq!(body["stream"], true);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "say hi");
    let tool_names: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, vec!["read", "write", "edit", "bash"]);
}

#[test]
fn tool_call_loop_executes_and_feeds_back() {
    let (base_url, requests) = start_mock_api(vec![
        sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo from-tool\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]),
        sse(&[
            r#"{"choices":[{"delta":{"content":"all done"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]),
    ]);

    let output = pi()
        .args([
            "-p",
            "run echo",
            "--base-url",
            &base_url,
            "--api-key",
            "test-key",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("all done"), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bash(echo from-tool)"), "stderr: {stderr}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    let messages = second["messages"].as_array().unwrap();
    // system, user, assistant(tool_calls), tool
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");
    let tool_output = messages[3]["content"].as_str().unwrap();
    assert!(
        tool_output.contains("from-tool"),
        "tool output: {tool_output}"
    );
}

#[test]
fn json_mode_emits_jsonl() {
    let (base_url, _requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
    ])]);

    let output = pi()
        .args([
            "--json",
            "-p",
            "hi",
            "--base-url",
            &base_url,
            "--api-key",
            "test-key",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("bad line {line}: {e}")))
        .collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types.first(), Some(&"session"));
    assert!(types.contains(&"assistant"));
    assert!(types.contains(&"usage"));
    let result = lines.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["outcome"], "completed");
    assert_eq!(result["usage"]["input_tokens"], 3);
}

#[test]
fn missing_api_key_fails_with_a_clear_message() {
    let output = pi().args(["-p", "hi"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DEEPSEEK_API_KEY"), "stderr: {stderr}");
}

#[test]
fn help_and_version() {
    let help = pi().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("USAGE"));

    let version = pi().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("pi"));
}
