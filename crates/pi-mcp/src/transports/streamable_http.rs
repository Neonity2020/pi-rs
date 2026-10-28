//! MCP Streamable HTTP transport for JSON and SSE POST responses and the
//! optional server-to-client GET stream. GET reconnects with Last-Event-ID;
//! Response streams can resume via GET with Last-Event-ID. OAuth and
//! server-initiated requests are not yet implemented.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{Transport, TransportEvent, parse_frame};
use crate::protocol::jsonrpc::JsonRpcRequest;

pub struct StreamableHttpTransport {
	pub url: String,
	pub headers: Vec<(String, String)>,
	client: reqwest::Client,
	session_id: Option<String>,
	protocol_version: Option<String>,
	tx: mpsc::Sender<TransportEvent>,
	fatal_tx: tokio::sync::watch::Sender<Option<String>>,
	fatal_rx: tokio::sync::watch::Receiver<Option<String>>,
	rx: mpsc::Receiver<TransportEvent>,
	tasks: Vec<tokio::task::JoinHandle<()>>,
	closed: bool,
}

impl StreamableHttpTransport {
	pub fn new(url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
		let (tx, rx) = mpsc::channel(128);
		let (fatal_tx, fatal_rx) = tokio::sync::watch::channel(None);
		Self {
			url: url.into(),
			headers,
			client: reqwest::Client::new(),
			session_id: None,
			protocol_version: None,
			tx,
			fatal_tx,
			fatal_rx,
			rx,
			tasks: Vec::new(),
			closed: false,
		}
	}

	fn request(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
		let mut request = self.client.request(method, &self.url);
		for (name, value) in &self.headers {
			request = request.header(name, value);
		}
		if let Some(id) = &self.session_id {
			request = request.header("Mcp-Session-Id", id);
		}
		if let Some(version) = &self.protocol_version {
			request = request.header("MCP-Protocol-Version", version);
		}
		request
	}

	fn start_get(&mut self) {
		let request = self
			.request(reqwest::Method::GET)
			.header("Accept", "text/event-stream");
		self.tasks.retain(|task| !task.is_finished());
		let tx = self.tx.clone();
		let fatal_tx = self.fatal_tx.clone();
		self.tasks.push(tokio::spawn(async move {
			let mut cursor: Option<String> = None;
			let mut retry: Option<std::time::Duration> = None;
			let mut delay = std::time::Duration::from_secs(1);
			let mut attempt = 0u32;
			loop {
				let Some(mut next) = request.try_clone() else {
					break;
				};
				if let Some(id) = &cursor {
					next = next.header("Last-Event-ID", id);
				}
				match next.send().await {
					Ok(response) if response.status().as_u16() == 405 => break,
					Ok(response) if response.status().is_success() && is_sse(&response) => {
						let outcome = read_sse(response, tx.clone(), None, cursor).await;
						cursor = outcome.cursor;
						retry = outcome.retry;
						// A stream that delivered events counts as healthy.
						if outcome.received {
							attempt = 0;
						}
					}
					Ok(response)
						if response.status().is_client_error()
							&& !matches!(response.status().as_u16(), 408 | 429) =>
					{
						let _ =
							fatal_tx.send(Some(format!("MCP GET failed: {}", response.status())));
						break;
					}
					Ok(response) if response.status().is_success() => {
						let _ = fatal_tx.send(Some("MCP GET expected text/event-stream".into()));
						break;
					}
					_ => {}
				}
				if attempt >= 5 {
					let _ = fatal_tx.send(Some(
						"MCP server-to-client stream dropped and could not be reopened".into(),
					));
					break;
				}
				// A server `retry:` field wins over the local backoff.
				tokio::time::sleep(retry.unwrap_or(delay)).await;
				attempt += 1;
				delay = (delay * 2).min(std::time::Duration::from_secs(30));
			}
		}));
	}
}

fn is_sse(response: &reqwest::Response) -> bool {
	response
		.headers()
		.get(reqwest::header::CONTENT_TYPE)
		.and_then(|h| h.to_str().ok())
		.is_some_and(|s| {
			s.split(';')
				.next()
				.is_some_and(|s| s.trim().eq_ignore_ascii_case("text/event-stream"))
		})
}

async fn emit_frame(
	tx: &mpsc::Sender<TransportEvent>,
	text: &str,
	expected_id: Option<&serde_json::Value>,
) -> bool {
	match parse_frame(text) {
		Ok(frame) => {
			let has_id = expected_id.is_some()
				&& frame.id.as_ref() == expected_id
				&& (frame.result.is_some() || frame.error.is_some());
			let _ = tx.send(TransportEvent::Message(Box::new(frame))).await;
			has_id
		}
		Err(error) => {
			let _ = tx.send(TransportEvent::Error(error.to_string())).await;
			false
		}
	}
}

struct SseOutcome {
	cursor: Option<String>,
	answered: bool,
	received: bool,
	retry: Option<std::time::Duration>,
	fatal: bool,
}

async fn read_sse(
	mut response: reqwest::Response,
	tx: mpsc::Sender<TransportEvent>,
	expected_id: Option<serde_json::Value>,
	mut cursor: Option<String>,
) -> SseOutcome {
	let mut pending = Vec::<u8>::new();
	let mut data = Vec::<String>::new();
	let mut data_bytes = 0usize;
	let mut event_id: Option<String> = None;
	let mut received_response = false;
	let mut received = false;
	let mut retry = None;
	let mut fatal = false;
	loop {
		match response.chunk().await {
			Ok(Some(chunk)) => {
				pending.extend_from_slice(&chunk);
				if pending.len() > 1024 * 1024 {
					let _ = tx
						.send(TransportEvent::Error("MCP SSE line exceeds 1 MiB".into()))
						.await;
					fatal = true;
					break;
				}
				while let Some(end) = pending.iter().position(|b| *b == b'\n') {
					let mut line = pending.drain(..=end).collect::<Vec<_>>();
					line.pop();
					if line.last() == Some(&b'\r') {
						line.pop();
					}
					if line.is_empty() {
						received = true;
						if let Some(id) = event_id.take() {
							cursor = Some(id);
						}
						if !data.is_empty() {
							received_response |=
								emit_frame(&tx, &data.join("\n"), expected_id.as_ref()).await;
							data.clear();
							data_bytes = 0;
						}
					} else if let Some(value) = line.strip_prefix(b"id:") {
						let value = value.strip_prefix(b" ").unwrap_or(value);
						if !value.contains(&0) {
							event_id = Some(String::from_utf8_lossy(value).into_owned());
						}
					} else if let Some(value) = line.strip_prefix(b"retry:") {
						let value = value.strip_prefix(b" ").unwrap_or(value);
						if let Ok(value) = std::str::from_utf8(value)
							&& let Ok(ms) = value.parse::<u64>()
						{
							retry = Some(std::time::Duration::from_millis(ms.min(30_000)));
						}
					} else if let Some(value) = line.strip_prefix(b"data:") {
						let value = value.strip_prefix(b" ").unwrap_or(value);
						data_bytes = data_bytes.saturating_add(value.len() + 1);
						if data_bytes > 1024 * 1024 {
							let _ = tx
								.send(TransportEvent::Error("MCP SSE event exceeds 1 MiB".into()))
								.await;
							return SseOutcome {
								cursor,
								answered: received_response,
								received,
								retry,
								fatal: true,
							};
						}
						data.push(String::from_utf8_lossy(value).into_owned());
					}
				}
			}
			Ok(None) => break,
			Err(error) => {
				let _ = tx
					.send(TransportEvent::Error(format!(
						"MCP SSE read failed: {error}"
					)))
					.await;
				break;
			}
		}
	}
	// SSE events dispatch only after a blank line; an incomplete final frame is invalid.
	SseOutcome {
		cursor,
		answered: received_response,
		received,
		retry,
		fatal,
	}
}

#[async_trait]
impl Transport for StreamableHttpTransport {
	async fn send(&mut self, request: &JsonRpcRequest) -> crate::Result<()> {
		if self.closed {
			return Err(crate::Error::Closed);
		}
		if let Some(error) = self.fatal_rx.borrow().clone() {
			return Err(crate::Error::Transport(error));
		}
		self.tasks.retain(|task| !task.is_finished());
		let post = self
			.request(reqwest::Method::POST)
			.header("Accept", "application/json, text/event-stream")
			.json(request)
			.send();
		let response = tokio::select! { biased;
			changed = self.fatal_rx.changed() => {
				if changed.is_ok() && let Some(error) = self.fatal_rx.borrow().clone() {
					return Err(crate::Error::Transport(error));
				}
				return Err(crate::Error::Closed);
			}
			result = post => result.map_err(|e| crate::Error::Transport(e.to_string()))?,
		};
		if !response.status().is_success() {
			let status = response.status();
			let body = response.text().await.unwrap_or_default();
			let reason = if status.as_u16() == 404 && self.session_id.is_some() {
				"MCP session expired"
			} else {
				"MCP POST failed"
			};
			return Err(crate::Error::Transport(format!(
				"{reason}: {status}: {body}"
			)));
		}
		if let Some(id) = response.headers().get("mcp-session-id") {
			self.session_id = Some(
				id.to_str()
					.map_err(|e| crate::Error::Protocol(e.to_string()))?
					.to_owned(),
			);
		}
		if response.status().as_u16() == 202 {
			if request.id.is_some() {
				return Err(crate::Error::Protocol(
					"request returned 202 without a response".into(),
				));
			}
		} else if is_sse(&response) {
			let tx = self.tx.clone();
			let expected = request.id.clone();
			let resume = self
				.request(reqwest::Method::GET)
				.header("Accept", "text/event-stream");
			self.tasks.push(tokio::spawn(async move {
				let mut outcome = read_sse(response, tx.clone(), expected.clone(), None).await;
				let mut attempt = 0u32;
				while expected.is_some()
					&& !outcome.answered
					&& !outcome.fatal
					&& outcome.cursor.is_some()
					&& attempt < 5
				{
					if outcome.received {
						attempt = 0;
					}
					let delay = outcome
						.retry
						.unwrap_or_else(|| std::time::Duration::from_secs(1u64 << attempt.min(4)));
					attempt += 1;
					tokio::time::sleep(delay).await;
					let Some(next) = resume.try_clone() else {
						break;
					};
					let response = next
						.header("Last-Event-ID", outcome.cursor.as_deref().unwrap())
						.send()
						.await;
					match response {
						Ok(response) if response.status().is_success() && is_sse(&response) => {
							outcome =
								read_sse(response, tx.clone(), expected.clone(), outcome.cursor)
									.await;
						}
						Ok(response)
							if response.status().as_u16() == 408
								|| response.status().as_u16() == 429
								|| response.status().is_server_error() =>
						{
							outcome.received = false;
						}
						Err(_) => {
							outcome.received = false;
						}
						_ => break,
					}
				}
				if let Some(id) = expected
					&& !outcome.answered
				{
					let frame = crate::protocol::jsonrpc::JsonRpcResponse {
						jsonrpc: "2.0".into(),
						id: Some(id),
						result: None,
						error: Some(crate::protocol::jsonrpc::JsonRpcError {
							code: -32603,
							message: "MCP response stream failed: stream ended without a response"
								.into(),
							data: None,
						}),
					};
					let _ = tx.send(TransportEvent::Message(Box::new(frame))).await;
				}
			}));
		} else {
			let content_type = response
				.headers()
				.get(reqwest::header::CONTENT_TYPE)
				.and_then(|v| v.to_str().ok())
				.unwrap_or("");
			if !content_type
				.split(';')
				.next()
				.is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
			{
				return Err(crate::Error::Protocol(format!(
					"unsupported MCP response content type: {content_type}"
				)));
			}
			let body = response
				.text()
				.await
				.map_err(|e| crate::Error::Transport(e.to_string()))?;
			// JSON replies may be a single frame or a JSON-RPC batch array;
			// every frame is forwarded and the client matches by id (as
			// upstream `emitMessage` does).
			let parsed: serde_json::Value = serde_json::from_str(&body)
				.map_err(|e| crate::Error::Protocol(format!("invalid MCP response: {e}")))?;
			let frames = match parsed {
				serde_json::Value::Array(items) => items,
				item => vec![item],
			};
			for item in frames {
				let frame: crate::protocol::jsonrpc::JsonRpcResponse =
					serde_json::from_value(item).map_err(crate::Error::Json)?;
				self.tx
					.try_send(TransportEvent::Message(Box::new(frame)))
					.map_err(|_| crate::Error::Transport("MCP event queue full".into()))?;
			}
		}
		if request.method == "notifications/initialized" {
			self.start_get();
		}
		Ok(())
	}

	async fn recv(&mut self) -> crate::Result<Option<TransportEvent>> {
		if self.closed {
			return Ok(None);
		}
		if let Some(error) = self.fatal_rx.borrow().clone() {
			return Err(crate::Error::Transport(error));
		}
		let event = tokio::select! { biased;
			changed = self.fatal_rx.changed() => {
				if changed.is_ok() && let Some(error) = self.fatal_rx.borrow().clone() { return Err(crate::Error::Transport(error)); }
				return Err(crate::Error::Closed);
			}
			event = self.rx.recv() => event,
		};
		if self.protocol_version.is_none()
			&& let Some(TransportEvent::Message(frame)) = &event
		{
			self.protocol_version = frame
				.result
				.as_ref()
				.and_then(|v| v.get("protocolVersion"))
				.and_then(|v| v.as_str())
				.map(str::to_owned);
		}
		Ok(event)
	}

	async fn close(&mut self) -> crate::Result<()> {
		if self.closed {
			return Ok(());
		}
		self.closed = true;
		for task in self.tasks.drain(..) {
			task.abort();
		}
		if self.session_id.is_some() {
			let _ = tokio::time::timeout(
				std::time::Duration::from_secs(1),
				self.request(reqwest::Method::DELETE).send(),
			)
			.await;
		}
		Ok(())
	}

	fn describe(&self) -> String {
		format!("streamable-http: {}", self.url)
	}
	fn supports_server_messages(&self) -> bool {
		// The GET stream forwards server→client frames; server *requests*
		// surface as envelopes without result/error and are skipped by the
		// client for now.
		true
	}
}

impl Drop for StreamableHttpTransport {
	fn drop(&mut self) {
		for task in self.tasks.drain(..) {
			task.abort();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{Read, Write};
	use std::net::TcpListener;

	fn read_request(stream: &mut std::net::TcpStream) -> String {
		stream
			.set_read_timeout(Some(std::time::Duration::from_secs(3)))
			.unwrap();
		let mut bytes = Vec::new();
		let mut chunk = [0; 4096];
		loop {
			let n = stream.read(&mut chunk).unwrap();
			if n == 0 {
				break;
			}
			bytes.extend_from_slice(&chunk[..n]);
			if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
				let headers = String::from_utf8_lossy(&bytes[..end]);
				let length = headers
					.lines()
					.find_map(|l| {
						l.to_ascii_lowercase()
							.strip_prefix("content-length: ")
							.and_then(|v| v.parse::<usize>().ok())
					})
					.unwrap_or(0);
				if bytes.len() >= end + 4 + length {
					break;
				}
			}
		}
		String::from_utf8(bytes).unwrap()
	}

	fn respond(stream: &mut std::net::TcpStream, status: &str, headers: &str, body: &str) {
		write!(
			stream,
			"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
			body.len()
		)
		.unwrap();
	}

	#[tokio::test]
	async fn json_handshake_session_headers_get_and_delete() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let (get_tx, get_rx) = tokio::sync::oneshot::channel();
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let init = read_request(&mut s);
			assert!(init.contains("\"method\":\"initialize\""));
			respond(
				&mut s,
				"200 OK",
				"Content-Type: application/json\r\nMcp-Session-Id: session-1\r\n",
				r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
			);
			let (mut s, _) = listener.accept().unwrap();
			let initialized = read_request(&mut s).to_ascii_lowercase();
			assert!(initialized.contains("mcp-session-id: session-1"));
			assert!(initialized.contains("mcp-protocol-version: 2025-06-18"));
			respond(&mut s, "202 Accepted", "", "");
			let (mut s, _) = listener.accept().unwrap();
			let get = read_request(&mut s).to_ascii_lowercase();
			assert!(get.starts_with("get "));
			assert!(get.contains("mcp-session-id: session-1"));
			respond(&mut s, "405 Method Not Allowed", "", "");
			let _ = get_tx.send(());
			let (mut s, _) = listener.accept().unwrap();
			let delete = read_request(&mut s).to_ascii_lowercase();
			assert!(delete.starts_with("delete "));
			assert!(delete.contains("mcp-session-id: session-1"));
			respond(&mut s, "204 No Content", "", "");
		});
		let mut client = crate::client::McpClient::new(
			"test",
			Box::new(StreamableHttpTransport::new(url, vec![])),
		);
		client.initialize("test").await.unwrap();
		tokio::time::timeout(std::time::Duration::from_secs(3), get_rx)
			.await
			.unwrap()
			.unwrap();
		client.close().await.unwrap();
		server.join().unwrap();
	}

	#[tokio::test]
	async fn sse_response_handles_split_chunks_and_crlf() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			let body =
				"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":\r\ndata: {\"ok\":true}}\r\n\r\n";
			write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
			for chunk in body.as_bytes().chunks(3) {
				s.write_all(chunk).unwrap();
			}
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport
			.send(&JsonRpcRequest::request(7, "test", serde_json::Value::Null))
			.await
			.unwrap();
		match tokio::time::timeout(std::time::Duration::from_secs(3), transport.recv())
			.await
			.unwrap()
			.unwrap()
			.unwrap()
		{
			TransportEvent::Message(frame) => assert_eq!(frame.result.unwrap()["ok"], true),
			other => panic!("unexpected event: {other:?}"),
		}
		server.join().unwrap();
	}

	#[tokio::test]
	async fn session_expiry_is_reported() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(&mut s, "404 Not Found", "", "expired");
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport.session_id = Some("old".into());
		let error = transport
			.send(&JsonRpcRequest::request(1, "test", serde_json::Value::Null))
			.await
			.unwrap_err();
		assert!(error.to_string().contains("session expired"));
		server.join().unwrap();
	}

	#[tokio::test]
	async fn batched_json_responses_are_unpacked_in_order() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(
				&mut s,
				"200 OK",
				"Content-Type: application/json\r\n",
				r#"[{"jsonrpc":"2.0","id":7,"result":{"n":1}},{"jsonrpc":"2.0","id":8,"result":{"n":2}}]"#,
			);
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport
			.send(&JsonRpcRequest::request(7, "test", serde_json::Value::Null))
			.await
			.unwrap();
		// Both frames surface even though only the first matches the request;
		// the client does the id matching (upstream `emitMessage`).
		for expected in [1, 2] {
			match tokio::time::timeout(std::time::Duration::from_secs(3), transport.recv())
				.await
				.unwrap()
				.unwrap()
				.unwrap()
			{
				TransportEvent::Message(frame) => assert_eq!(frame.result.unwrap()["n"], expected),
				other => panic!("unexpected event: {other:?}"),
			}
		}
		server.join().unwrap();
	}

	#[tokio::test]
	async fn get_stream_delivers_events_after_initialized() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(&mut s, "202 Accepted", "", "");
			let (mut s, _) = listener.accept().unwrap();
			let request = read_request(&mut s);
			assert!(request.starts_with("GET "));
			respond(
				&mut s,
				"200 OK",
				"Content-Type: text/event-stream\r\n",
				"data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{\"ok\":true}}\n\n",
			);
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport
			.send(&JsonRpcRequest::notification(
				"notifications/initialized",
				serde_json::json!({}),
			))
			.await
			.unwrap();
		match tokio::time::timeout(std::time::Duration::from_secs(3), transport.recv())
			.await
			.unwrap()
			.unwrap()
			.unwrap()
		{
			TransportEvent::Message(frame) => assert_eq!(frame.result.unwrap()["ok"], true),
			other => panic!("unexpected event: {other:?}"),
		}
		server.join().unwrap();
	}

	#[tokio::test]
	async fn get_reconnects_with_last_event_id() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let (done_tx, done_rx) = tokio::sync::oneshot::channel();
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(
				&mut s,
				"200 OK",
				"Content-Type: text/event-stream\r\n",
				"id: event-42\ndata: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\n",
			);
			let (mut s, _) = listener.accept().unwrap();
			let request = read_request(&mut s).to_ascii_lowercase();
			assert!(request.contains("last-event-id: event-42"));
			respond(&mut s, "405 Method Not Allowed", "", "");
			let _ = done_tx.send(());
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport.start_get();
		tokio::time::timeout(std::time::Duration::from_secs(3), done_rx)
			.await
			.unwrap()
			.unwrap();
		transport.close().await.unwrap();
		server.join().unwrap();
	}

	#[tokio::test]
	async fn oversized_multiline_event_terminates_request() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			let body = "data: ".to_string() + &"x".repeat(1024) + "\n";
			let response = body.repeat(1025);
			let _ = write!(
				s,
				"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
				response.len(),
				response
			);
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport
			.send(&JsonRpcRequest::request(7, "test", serde_json::Value::Null))
			.await
			.unwrap();
		let first = tokio::time::timeout(std::time::Duration::from_secs(3), transport.recv())
			.await
			.unwrap()
			.unwrap()
			.unwrap();
		assert!(
			matches!(first, TransportEvent::Error(message) if message.contains("event exceeds"))
		);
		assert!(matches!(
			transport.recv().await.unwrap(),
			Some(TransportEvent::Message(frame)) if frame.id == Some(serde_json::json!(7)) && frame.error.is_some()
		));
		server.join().unwrap();
	}

	#[tokio::test]
	async fn fatal_get_error_bypasses_full_bounded_event_queue() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(&mut s, "404 Not Found", "", "expired");
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		for _ in 0..128 {
			transport
				.tx
				.try_send(TransportEvent::Error("queued".into()))
				.unwrap();
		}
		assert!(transport.tx.try_send(TransportEvent::Closed).is_err());
		transport.start_get();
		tokio::time::timeout(std::time::Duration::from_secs(3), async {
			loop {
				if transport.fatal_rx.borrow().is_some() {
					break;
				}
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		let error = transport.recv().await.unwrap_err();
		assert!(error.to_string().contains("404"));
		server.join().unwrap();
	}

	#[tokio::test]
	async fn completed_stream_tasks_are_pruned() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(&mut s, "405 Method Not Allowed", "", "");
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		for _ in 0..20 {
			transport.tasks.push(tokio::spawn(async {}));
		}
		tokio::task::yield_now().await;
		transport.start_get();
		assert_eq!(transport.tasks.len(), 1);
		tokio::time::timeout(std::time::Duration::from_secs(3), async {
			while !transport.tasks[0].is_finished() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		server.join().unwrap();
	}
	#[tokio::test]
	async fn post_stream_resumes_with_cursor_and_delivers_response() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			respond(
				&mut s,
				"200 OK",
				"Content-Type: text/event-stream\r\nMcp-Session-Id: abc\r\n",
				"id: cursor-7\nretry: 1\n\n",
			);
			let (mut s, _) = listener.accept().unwrap();
			let get = read_request(&mut s).to_ascii_lowercase();
			assert!(get.starts_with("get "));
			assert!(get.contains("last-event-id: cursor-7"));
			assert!(get.contains("mcp-session-id: abc"));
			respond(
				&mut s,
				"200 OK",
				"Content-Type: text/event-stream\r\n",
				"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
			);
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		transport
			.send(&JsonRpcRequest::request(7, "test", serde_json::Value::Null))
			.await
			.unwrap();
		let event = tokio::time::timeout(std::time::Duration::from_secs(3), transport.recv())
			.await
			.unwrap()
			.unwrap()
			.unwrap();
		assert!(
			matches!(event, TransportEvent::Message(frame) if frame.id == Some(serde_json::json!(7)) && frame.result.clone().unwrap()["ok"] == true)
		);
		server.join().unwrap();
	}

	#[tokio::test]
	async fn fatal_get_interrupts_post_awaiting_headers() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
		let (release_tx, release_rx) = std::sync::mpsc::channel();
		let server = std::thread::spawn(move || {
			let (mut s, _) = listener.accept().unwrap();
			let _ = read_request(&mut s);
			let _ = seen_tx.send(());
			let _ = release_rx.recv_timeout(std::time::Duration::from_secs(3));
		});
		let mut transport = StreamableHttpTransport::new(url, vec![]);
		let fatal = transport.fatal_tx.clone();
		let trigger = tokio::spawn(async move {
			seen_rx.await.unwrap();
			fatal.send(Some("GET expired".into())).unwrap();
		});
		let error = tokio::time::timeout(
			std::time::Duration::from_secs(2),
			transport.send(&JsonRpcRequest::request(1, "test", serde_json::Value::Null)),
		)
		.await
		.unwrap()
		.unwrap_err();
		assert!(error.to_string().contains("GET expired"));
		let _ = release_tx.send(());
		trigger.await.unwrap();
		server.join().unwrap();
	}
}
