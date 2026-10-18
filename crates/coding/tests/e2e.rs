//! End-to-end tests: run the real `pi-rs` binary against a local mock of the
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
    start_mock_api_responses(
        sse_bodies
            .into_iter()
            .map(|body| (200u16, "text/event-stream".to_string(), body))
            .collect(),
    )
}

fn start_mock_api_responses(
    responses: Vec<(u16, String, String)>,
) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    start_mock_api_with_hook(responses, || {})
}

fn start_mock_api_with_hook(
    responses: Vec<(u16, String, String)>,
    mut before_response: impl FnMut() + Send + 'static,
) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();

    std::thread::spawn(move || {
        for (status, content_type, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            if let Some(request) = read_request(&mut stream) {
                recorded.lock().unwrap().push(request);
            }
            before_response();
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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

fn pi_rs() -> Command {
    pi_in(&test_dir())
}

/// Isolate a run from the developer's machine: a fresh home for sessions and
/// auth.json, plus a matching working directory so a stray `.env` cannot leak
/// into a test. Reuse the same home to test state persisted across runs.
fn pi_in(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi-rs"));
    command.env("PI_RS_HOME", home);
    command.current_dir(home);
    command.env_remove("DEEPSEEK_API_KEY");
    command.env_remove("OPENAI_API_KEY");
    command
}

/// Feed stdin to the binary and collect its output (for `pi-rs login`).
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

fn session_path(home: &std::path::Path) -> std::path::PathBuf {
    let dir = std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .unwrap()
}

#[test]
fn persistence_failure_is_reported_and_prevents_tool_execution() {
    for (json_mode, tool_call) in [(false, false), (false, true), (true, false), (true, true)] {
        let home = test_dir();
        let sabotage_home = home.clone();
        let body = if tool_call {
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"write","arguments":"{\"file_path\":\"must-not-exist\",\"content\":\"bad\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ])
        } else {
            sse(&[r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}"#])
        };
        let (base_url, requests) =
            start_mock_api_with_hook(vec![(200, "text/event-stream".into(), body)], move || {
                let path = session_path(&sabotage_home);
                std::fs::rename(&path, path.with_extension("backup")).unwrap();
                std::fs::create_dir(path).unwrap();
            });
        let mut command = pi_in(&home);
        command.args(["-p", "test", "--base-url", &base_url, "--api-key", "fake"]);
        if json_mode {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if json_mode {
            let result: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
            assert_eq!(result["outcome"], "error", "{stdout}");
            assert!(result["message"]
                .as_str()
                .unwrap()
                .contains("session persistence failed"));
        } else {
            assert!(stderr.contains("session persistence failed"), "{stderr}");
            assert!(!stderr.contains("— done"));
        }
        assert!(
            !home.join("must-not-exist").exists(),
            "tool ran after failed assistant persistence"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn torn_session_tail_survives_two_binary_resumes() {
    let home = test_dir();
    let body = sse(&[r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}"#]);
    let (base_url, requests) = start_mock_api(vec![body.clone(), body.clone(), body]);
    let run = |prompt: &str, resume: bool| {
        let mut command = pi_in(&home);
        command.args(["-p", prompt, "--base-url", &base_url, "--api-key", "fake"]);
        if resume {
            command.arg("-c");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run("one", false);
    let path = session_path(&home);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"id\":\"torn")
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    run("two", true);
    run("three", true);
    let requests = requests.lock().unwrap();
    let messages = requests[2].body["messages"].as_array().unwrap();
    let users: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(users, ["one", "two", "three"]);
    let backup = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "bak"))
        .unwrap();
    assert_eq!(std::fs::read(backup).unwrap(), original);
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

    let output = pi_rs()
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
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("# Long-term memory"));
    assert!(system.contains("(0 notes)"));
}

#[test]
fn memory_is_recalled_in_the_system_prompt() {
    let home = test_dir();
    std::fs::write(
        home.join("MEMORY.md"),
        "# Long-term memory\n\n- (2026-10-17) prefers short answers\n",
    )
    .unwrap();
    let cwd = home.canonicalize().unwrap();
    let slug = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-");
    std::fs::create_dir_all(home.join("memory")).unwrap();
    std::fs::write(
        home.join("memory").join(format!("{slug}.md")),
        "- (2026-10-16) project uses cargo test\n",
    )
    .unwrap();

    let (base_url, requests) = start_mock_api(vec![sse(&[
        r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])]);

    let output = pi_in(&home)
        .args(["-p", "hi", "--base-url", &base_url, "--api-key", "test-key"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = requests.lock().unwrap();
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("# Long-term memory"), "system: {system}");
    assert!(system.contains("prefers short answers"), "system: {system}");
    assert!(
        system.contains("project uses cargo test"),
        "system: {system}"
    );
    assert!(system.contains("MEMORY.md"), "system: {system}");
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

    let output = pi_rs()
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

    let output = pi_rs()
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
    let output = pi_rs().args(["-p", "hi"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DEEPSEEK_API_KEY"), "stderr: {stderr}");
    // The fix should be discoverable, not just the env var name.
    assert!(stderr.contains("pi-rs login"), "stderr: {stderr}");
}

#[test]
fn blank_env_key_is_treated_as_missing() {
    // An untouched .env.example copy leaves DEEPSEEK_API_KEY= blank; that
    // must read as "missing", not as an empty key sent to the API.
    let workdir = test_dir();
    std::fs::write(workdir.join(".env"), "DEEPSEEK_API_KEY=\n").unwrap();
    let output = pi_rs()
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
    let output = pi_rs()
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
    let output = pi_rs()
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
    let help = pi_rs().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("USAGE"));

    let version = pi_rs().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("pi-rs"));
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

#[test]
fn context_overflow_compacts_the_session_and_retries() {
    let home = test_dir();

    // Seed a session with a large tool result: 100k characters, which the
    // compaction cut can drop while keeping the rest of the conversation.
    let huge = serde_json::json!({
        "command": "yes x | head -c 200000 | tr -d '\\n'"
    })
    .to_string();
    let tool_call = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": { "name": "bash", "arguments": huge }
                }]
            }
        }]
    })
    .to_string();
    let (seed_url, _) = start_mock_api(vec![
        sse(&[
            &tool_call,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]),
        sse(&[
            r#"{"choices":[{"delta":{"content":"seeded"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]),
    ]);
    let seed = pi_in(&home)
        .args([
            "-p",
            "seed",
            "--base-url",
            &seed_url,
            "--api-key",
            "test-key",
        ])
        .output()
        .unwrap();
    assert!(
        seed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&seed.stderr)
    );

    // The next run overflows. The agent should summarize the old prefix, keep
    // the recent tail, retry, and persist a compaction entry.
    let (base_url, requests) = start_mock_api_responses(vec![
        (
            400,
            "application/json".into(),
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#.into(),
        ),
        (
            200,
            "text/event-stream".into(),
            sse(&[
                r###"{"choices":[{"delta":{"content":"## Goal\nrecovered"}}]}"###,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ),
        (
            200,
            "text/event-stream".into(),
            sse(&[
                r#"{"choices":[{"delta":{"content":"all done"}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ]),
        ),
    ]);

    let output = pi_in(&home)
        .args([
            "-c",
            "-p",
            "next",
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
    assert!(stderr.contains("compacted context"), "stderr: {stderr}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "overflow, summary, retry");
    let summary_request = requests[1].body["messages"][1]["content"].as_str().unwrap();
    assert!(summary_request.contains("<conversation>"));
    assert!(summary_request.contains("more characters truncated"));

    let retried = requests[2].body["messages"].as_array().unwrap();
    assert!(retried[1]["content"]
        .as_str()
        .unwrap()
        .contains("compacted into the following summary"));
    assert!(
        retried.iter().all(|message| message["content"]
            .as_str()
            .is_none_or(|text| text.len() < 10_000)),
        "the huge tool result must not be sent again"
    );

    let slug_dir = std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let session = std::fs::read_dir(slug_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(session).unwrap();
    assert!(text.contains("\"type\":\"compaction\""), "session: {text}");
    assert!(text.contains("recovered"), "session: {text}");
}
