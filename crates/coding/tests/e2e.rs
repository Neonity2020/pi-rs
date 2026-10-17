//! End-to-end tests: run the real `pi` binary against a local mock of the
//! DeepSeek chat completions API (SSE).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// One request as seen by the mock API: lowercased raw headers and JSON body.
struct RecordedRequest {
    headers: String,
    body: Value,
}

impl RecordedRequest {
    /// `name` must be lowercase, e.g. `"authorization"`.
    fn header(&self, name: &str) -> &str {
        self.headers
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
            .unwrap_or("")
    }
}

/// A minimal HTTP/1.1 server that answers each request with one canned SSE
/// body and records each request (headers + body).
fn start_mock_api(sse_bodies: Vec<String>) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();

    std::thread::spawn(move || {
        for body in sse_bodies {
            let (mut stream, _) = listener.accept().unwrap();
            if let Some(request) = read_request(&mut stream) {
                recorded.lock().unwrap().push(request);
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

fn read_request(stream: &mut std::net::TcpStream) -> Option<RecordedRequest> {
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
            let body = serde_json::from_slice(&buffer[body_start..body_start + length]).ok()?;
            return Some(RecordedRequest { headers, body });
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
    pi_in(&test_dir())
}

/// Isolate a run from the developer's machine: a fresh home for sessions and
/// auth.json, plus a matching working directory so a stray `.env` cannot leak
/// into a test. Reuse the same home to test state persisted across runs.
fn pi_in(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command.env("PI_RS_HOME", home);
    command.current_dir(home);
    command.env_remove("DEEPSEEK_API_KEY");
    command.env_remove("OPENAI_API_KEY");
    command
}

/// Feed stdin to the binary and collect its output (for `pi login`).
fn run_with_stdin(mut command: Command, input: &str) -> std::process::Output {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// One-shot HTTP mock for the `GET /models` key check during login.
fn start_models_api(status: u16, body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Drain through the header terminator before answering; closing
            // with unread bytes would send a TCP RST and can race the client.
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            while find(&buffer, b"\r\n\r\n").is_none() {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
            }
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                403 => "Forbidden",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

fn test_dir() -> std::path::PathBuf {
    // Parallel tests can hit the same clock tick; the counter keeps their
    // `PI_RS_HOME` (and therefore auth.json) separate.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let home = std::env::temp_dir().join(format!(
        "pi-rs-e2e-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
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
    let body = &requests[0].body;
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
    let second = &requests[1].body;
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
    // The fix should be discoverable, not just the env var name.
    assert!(stderr.contains("pi login"), "stderr: {stderr}");
}

#[test]
fn blank_env_key_is_treated_as_missing() {
    // An untouched .env.example copy leaves DEEPSEEK_API_KEY= blank; that
    // must read as "missing", not as an empty key sent to the API.
    let workdir = test_dir();
    std::fs::write(workdir.join(".env"), "DEEPSEEK_API_KEY=\n").unwrap();
    let output = pi()
        .current_dir(&workdir)
        // A dead port keeps a regression fast instead of dialing the real API.
        .args(["-p", "hi", "--base-url", "http://127.0.0.1:1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DEEPSEEK_API_KEY"), "stderr: {stderr}");
}

#[test]
fn dotenv_file_supplies_the_api_key() {
    let (base_url, requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"content":"from dotenv"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])]);

    // No --api-key and no environment variable: the key must come from .env.
    let workdir = test_dir();
    std::fs::write(workdir.join(".env"), "DEEPSEEK_API_KEY=key-from-dotenv\n").unwrap();
    let output = pi()
        .current_dir(&workdir)
        .args(["-p", "hi", "--base-url", &base_url])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = requests.lock().unwrap();
    // Headers are recorded lowercased, hence the lowercase "bearer".
    assert_eq!(
        requests[0].header("authorization"),
        "bearer key-from-dotenv"
    );
}

#[test]
fn real_env_beats_the_dotenv_file() {
    let (base_url, requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])]);

    let workdir = test_dir();
    std::fs::write(workdir.join(".env"), "DEEPSEEK_API_KEY=key-from-dotenv\n").unwrap();
    let output = pi()
        .current_dir(&workdir)
        .env("DEEPSEEK_API_KEY", "key-from-env")
        .args(["-p", "hi", "--base-url", &base_url])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Headers are recorded lowercased, hence the lowercase "bearer".
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].header("authorization"), "bearer key-from-env");
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

#[test]
fn login_saves_auth_json_and_later_runs_use_it() {
    let home = test_dir();
    let models = start_models_api(200, r#"{"object":"list","data":[]}"#);

    // The key arrives on stdin; no environment variable is set.
    let output = run_with_stdin(
        {
            let mut command = pi_in(&home);
            command.args(["login", "--base-url", &models]);
            command
        },
        "sk-from-stdin\n",
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let auth_path = home.join("auth.json");
    let auth: Value = serde_json::from_str(&std::fs::read_to_string(&auth_path).unwrap()).unwrap();
    assert_eq!(auth["deepseek"]["type"], "api_key");
    assert_eq!(auth["deepseek"]["key"], "sk-from-stdin");
    assert_eq!(auth["deepseek"]["base_url"], models);

    // The next run finds the key in auth.json and sends it to the API.
    let (base_url, requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"content":"from auth"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])]);
    let output = pi_in(&home)
        .args(["-p", "hi", "--base-url", &base_url])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("from auth"));

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("authorization"), "bearer sk-from-stdin");
}

#[test]
fn login_rejects_a_bad_key_without_writing_auth_json() {
    let home = test_dir();
    let models = start_models_api(401, r#"{"error":{"message":"invalid key"}}"#);

    let output = pi_in(&home)
        .args(["login", "--api-key", "sk-bad", "--base-url", &models])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rejected"), "stderr: {stderr}");
    assert!(!home.join("auth.json").exists(), "auth.json must not exist");
}

#[test]
fn env_key_beats_auth_json() {
    let home = test_dir();
    let models = start_models_api(200, r#"{"object":"list","data":[]}"#);
    let output = pi_in(&home)
        .args(["login", "--api-key", "sk-saved", "--base-url", &models])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (base_url, requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])]);
    let output = pi_in(&home)
        .env("DEEPSEEK_API_KEY", "sk-from-env")
        .args(["-p", "hi", "--base-url", &base_url])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].header("authorization"), "bearer sk-from-env");
}

#[test]
fn logout_removes_the_saved_credential() {
    let home = test_dir();
    let models = start_models_api(200, r#"{"object":"list","data":[]}"#);
    let output = pi_in(&home)
        .args(["login", "--api-key", "sk-saved", "--base-url", &models])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = pi_in(&home).args(["logout"]).output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("auth.json")).unwrap()).unwrap();
    assert!(auth.get("deepseek").is_none(), "auth.json: {auth}");
}
