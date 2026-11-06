//! Shared test-only helpers: a minimal HTTP/1.1 mock server over
//! `std::net::TcpListener` (the pattern the transport tests already use).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// Read one HTTP/1.1 request (headers + content-length body) from `stream`.
pub(crate) fn read_request(stream: &mut TcpStream) -> String {
	stream
		.set_read_timeout(Some(std::time::Duration::from_secs(3)))
		.unwrap();
	let mut bytes = Vec::new();
	let mut chunk = [0; 4096];
	loop {
		let read = stream.read(&mut chunk).unwrap();
		if read == 0 {
			break;
		}
		bytes.extend_from_slice(&chunk[..read]);
		if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
			let headers = String::from_utf8_lossy(&bytes[..end]);
			let length = headers
				.lines()
				.find_map(|line| {
					line.to_ascii_lowercase()
						.strip_prefix("content-length: ")
						.and_then(|value| value.parse::<usize>().ok())
				})
				.unwrap_or(0);
			if bytes.len() >= end + 4 + length {
				break;
			}
		}
	}
	String::from_utf8(bytes).unwrap()
}

/// Write one HTTP/1.1 response with `Connection: close` and close the
/// stream.
pub(crate) fn respond(stream: &mut TcpStream, status: &str, headers: &str, body: &str) {
	write!(
		stream,
		"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
		body.len()
	)
	.unwrap();
	let _ = stream.flush();
}

/// Bind an ephemeral loopback listener for a mock server thread.
pub(crate) fn listener() -> (TcpListener, String) {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	(listener, url)
}

/// One parsed mock request: method, path, and the raw request text (headers
/// plus body, lowercasing left to the test).
pub(crate) struct MockRequest {
	pub method: String,
	pub path: String,
	pub raw: String,
}

/// One mock response.
pub(crate) struct MockResponse {
	pub status: &'static str,
	pub headers: String,
	pub body: String,
}

impl MockResponse {
	pub fn json(status: &'static str, body: impl Into<String>) -> Self {
		Self {
			status,
			headers: "Content-Type: application/json\r\n".to_string(),
			body: body.into(),
		}
	}
}

/// A path-routing mock server: each accepted connection is parsed and
/// answered by `handler`; the server stops after `connections` requests.
/// The `url` is the `http://127.0.0.1:<port>` base.
pub(crate) fn serve(
	connections: usize,
	handler: Arc<dyn Fn(&MockRequest) -> MockResponse + Send + Sync>,
) -> (String, std::thread::JoinHandle<()>) {
	let (listener, url) = listener();
	let handle = std::thread::spawn(move || {
		for _ in 0..connections {
			let (mut stream, _) = listener.accept().unwrap();
			let raw = read_request(&mut stream);
			let mut parts = raw.split_whitespace();
			let method = parts.next().unwrap_or("").to_string();
			let path = parts.next().unwrap_or("").to_string();
			let request = MockRequest { method, path, raw };
			let response = handler(&request);
			respond(
				&mut stream,
				response.status,
				&response.headers,
				&response.body,
			);
		}
	});
	(url, handle)
}
