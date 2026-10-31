use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

#[test]
fn prompt_arguments_reach_provider_request() {
	let root = std::env::temp_dir().join(format!("pi-prompt-cli-{}", uuid::Uuid::new_v4()));
	let agent_dir = root.join("agent");
	std::fs::create_dir_all(&agent_dir).unwrap();
	std::fs::write(root.join("AGENTS.md"), "CONTEXT_DISABLED").unwrap();
	std::fs::write(root.join("prompt.txt"), "\u{feff}FILE_CUSTOM").unwrap();
	std::fs::write(root.join("append.txt"), "FILE_APPEND").unwrap();
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let addr = listener.local_addr().unwrap();
	let config = serde_json::json!({"providers":{"local":{
		"api":"openai-completions", "baseUrl":format!("http://{addr}/v1"),
		"apiKey":"test", "models":[{"id":"test-model"}]
	}}});
	std::fs::write(agent_dir.join("models.json"), config.to_string()).unwrap();
	let server = std::thread::spawn(move || {
		let (mut socket, _) = listener.accept().unwrap();
		socket
			.set_read_timeout(Some(std::time::Duration::from_secs(10)))
			.unwrap();
		let mut request = Vec::new();
		let mut chunk = [0; 4096];
		let (body_start, length) = loop {
			let n = socket.read(&mut chunk).unwrap();
			assert!(n > 0);
			request.extend_from_slice(&chunk[..n]);
			if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
				let header = String::from_utf8_lossy(&request[..pos]);
				let length = header
					.lines()
					.find_map(|line| {
						let (key, value) = line.split_once(':')?;
						key.eq_ignore_ascii_case("content-length")
							.then(|| value.trim().parse::<usize>().unwrap())
					})
					.unwrap();
				break (pos + 4, length);
			}
		};
		while request.len() < body_start + length {
			let n = socket.read(&mut chunk).unwrap();
			assert!(n > 0);
			request.extend_from_slice(&chunk[..n]);
		}
		let body: serde_json::Value =
			serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
		let headers = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
		let response = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
		write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
		(body, headers)
	});
	let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
		.current_dir(&root)
		.env("PI_RS_AGENT_DIR", &agent_dir)
		.stdin(Stdio::null())
		.args([
			"--no-mcp",
			"--no-session",
			"--no-context-files",
			"--system-prompt",
			"prompt.txt",
			"--append-system-prompt",
			"literal append",
			"--append-system-prompt",
			"append.txt",
			"--model",
			"local/test-model",
			"--api-key",
			"cli-test-key",
			"--print",
			"hello",
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let (body, headers) = server.join().unwrap();
	assert!(headers.contains("authorization: bearer cli-test-key"));
	assert!(
		!std::fs::read_to_string(agent_dir.join("models.json"))
			.unwrap()
			.contains("cli-test-key")
	);
	let message = body["messages"]
		.as_array()
		.unwrap()
		.iter()
		.find(|m| m["role"] == "system")
		.unwrap();
	let prompt = message["content"].as_str().unwrap();
	assert!(prompt.starts_with("FILE_CUSTOM\n\nliteral append\n\nFILE_APPEND"));
	assert!(!prompt.contains("CONTEXT_DISABLED"));
	std::fs::remove_dir_all(root).unwrap();
}
