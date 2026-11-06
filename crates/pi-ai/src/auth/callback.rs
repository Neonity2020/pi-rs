//! Loopback OAuth redirect handler — port of
//! `pi/packages/ai/src/auth/oauth/callback-server.ts` plus the page renderer
//! from `pi/packages/ai/src/utils/oauth-page.ts`.
//!
//! A std TcpListener thread accepts one browser GET, validates route/state,
//! and hands the authorization code to the async flow through a channel. The
//! completion result travels back to the thread so the browser page can show
//! exchange failures (upstream `complete` before page). `wait` is bridged
//! into async via `spawn_blocking`; whole-flow cancellation lives with the
//! flow (select on the interaction signal), matching upstream's abort wiring.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::types::AuthError;

const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 800" aria-hidden="true"><path fill="#F09082" d="M165.29 165.29H517.36V400H400V282.65H165.29Z"/><path fill="#4D9ABF" d="M165.29 282.65H282.65V400H400V517.36H282.65V634.72H165.29Z"/><path fill="#F1BE58" d="M517.36 400H634.72V634.72H517.36Z"/></svg>"##;

fn escape_html(value: &str) -> String {
	value
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
		.replace('\'', "&#39;")
}

fn render_page(heading: &str, message: &str, details: Option<&str>) -> String {
	let title = if heading.contains("failed") {
		"Authentication failed"
	} else {
		"Authentication successful"
	};
	let heading = escape_html(heading);
	let message = escape_html(message);
	let details = details.map(escape_html);
	format!(
		r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{title}</title>
  <style>
    :root {{
      --text: #fafafa;
      --text-dim: #a1a1aa;
      --page-bg: #09090b;
      --font-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, "Noto Sans", sans-serif, "Apple Color Emoji", "Segoe UI Emoji", "Segoe UI Symbol", "Noto Color Emoji";
      --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    }}
    * {{ box-sizing: border-box; }}
    html {{ color-scheme: dark; }}
    body {{
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 24px;
      background: var(--page-bg);
      color: var(--text);
      font-family: var(--font-sans);
      text-align: center;
    }}
    main {{
      width: 100%;
      max-width: 560px;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
    }}
    .logo {{
      width: 72px;
      height: 72px;
      display: block;
      margin-bottom: 24px;
    }}
    h1 {{
      margin: 0 0 10px;
      font-size: 28px;
      line-height: 1.15;
      font-weight: 650;
      color: var(--text);
    }}
    p {{
      margin: 0;
      line-height: 1.7;
      color: var(--text-dim);
      font-size: 15px;
    }}
    .details {{
      margin-top: 16px;
      font-family: var(--font-mono);
      font-size: 13px;
      color: var(--text-dim);
      white-space: pre-wrap;
      word-break: break-word;
    }}
  </style>
</head>
<body>
  <main>
    <div class="logo">{LOGO_SVG}</div>
    <h1>{heading}</h1>
    <p>{message}</p>
    {details_html}
  </main>
</body>
</html>"#,
		details_html = details
			.map(|d| format!(r#"<div class="details">{d}</div>"#))
			.unwrap_or_default(),
	)
}

pub fn oauth_success_html(message: &str) -> String {
	render_page("Authentication successful", message, None)
}

pub fn oauth_error_html(message: &str, details: Option<&str>) -> String {
	render_page("Authentication failed", message, details)
}

/// What the flow learns from the browser (`wait()` resolution).
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackOutcome {
	/// The provider redirected with a code. `complete_with` has already told
	/// the browser how the exchange went.
	Code(String),
	/// The provider redirected with an `error` parameter.
	ProviderError(String),
	/// `cancel()` was called before a callback arrived.
	Cancelled,
}

enum ThreadCommand {
	/// Deliver a completion page for the claimed code on the parked socket.
	Page { status: u16, html: String },
	/// Stop waiting unless a callback was claimed.
	Cancel,
	/// Shut the server down.
	Close,
}

struct ServerState {
	shutdown: AtomicBool,
	claimed: AtomicBool,
	/// The connection that carried the claimed code, parked until the flow's
	/// completion page arrives.
	parked: Mutex<Option<TcpStream>>,
}

pub struct OAuthCallbackServer {
	pub redirect_uri: String,
	port: u16,
	events: Mutex<Option<std::sync::mpsc::Receiver<CallbackOutcome>>>,
	control: std::sync::mpsc::Sender<ThreadCommand>,
	state: Arc<ServerState>,
}

impl OAuthCallbackServer {
	/// Bind `host:port` (`port` 0 picks a free one) and start serving. The
	/// returned [`OAuthCallbackServer::redirect_uri`] uses `redirect_host`
	/// (e.g. `localhost` when binding `127.0.0.1`).
	pub fn start(options: CallbackServerOptions) -> Result<Self, AuthError> {
		if options.signal.is_cancelled() {
			return Err(AuthError::Login("Login cancelled".to_string()));
		}
		let listener = TcpListener::bind((options.host.as_str(), options.port))
			.map_err(|e| AuthError::Login(format!("OAuth callback server failed to bind: {e}")))?;
		let port = listener
			.local_addr()
			.map_err(|e| AuthError::Login(e.to_string()))?
			.port();
		let redirect_host = options
			.redirect_host
			.unwrap_or_else(|| options.host.clone());
		let host_component = if redirect_host.contains(':') {
			format!("[{redirect_host}]")
		} else {
			redirect_host
		};
		let redirect_uri = format!("http://{host_component}:{port}{}", options.path);

		let (events_tx, events_rx) = std::sync::mpsc::channel::<CallbackOutcome>();
		let (control_tx, control_rx) = std::sync::mpsc::channel::<ThreadCommand>();
		let state = Arc::new(ServerState {
			shutdown: AtomicBool::new(false),
			claimed: AtomicBool::new(false),
			parked: Mutex::new(None),
		});

		let thread_state = Arc::clone(&state);
		let path = options.path;
		let provider_name = options.provider_name;
		std::thread::spawn(move || {
			let deadline = options
				.timeout_ms
				.map(|ms| Instant::now() + Duration::from_millis(ms));
			listener.set_nonblocking(true).ok();
			let mut settled = false;
			loop {
				if thread_state.shutdown.load(Ordering::SeqCst) {
					return;
				}
				if !settled
					&& let Some(deadline) = deadline
					&& Instant::now() >= deadline
				{
					let _ = events_tx.send(CallbackOutcome::ProviderError(format!(
						"{provider_name} sign-in timed out"
					)));
					settled = true;
				}
				while let Ok(command) = control_rx.try_recv() {
					match command {
						ThreadCommand::Page { status, html } => {
							if let Some(stream) = thread_state.parked.lock().unwrap().take() {
								write_page(stream, status, &html);
							}
						}
						ThreadCommand::Cancel => {
							if !thread_state.claimed.load(Ordering::SeqCst) && !settled {
								let _ = events_tx.send(CallbackOutcome::Cancelled);
								settled = true;
							}
						}
						ThreadCommand::Close => {
							thread_state.shutdown.store(true, Ordering::SeqCst);
							return;
						}
					}
				}
				if let Ok((stream, _addr)) = listener.accept() {
					match handle_connection(
						stream,
						&path,
						options.state.as_deref(),
						&provider_name,
						&thread_state,
						&events_tx,
					) {
						ConnectionResult::Deferred => settled = true,
						ConnectionResult::Rejected => {}
					}
				}
				std::thread::sleep(Duration::from_millis(20));
			}
		});

		Ok(Self {
			redirect_uri,
			port,
			events: Mutex::new(Some(events_rx)),
			control: control_tx,
			state,
		})
	}

	pub fn port(&self) -> u16 {
		self.port
	}

	/// Wait for the browser outcome (bridged into async). A `Cancelled`
	/// resolution lets the flow fall back to the manual prompt.
	pub async fn wait(&self) -> CallbackOutcome {
		let receiver = self.events.lock().unwrap().take();
		let Some(receiver) = receiver else {
			return CallbackOutcome::Cancelled;
		};
		tokio::task::spawn_blocking(move || receiver.recv().unwrap_or(CallbackOutcome::Cancelled))
			.await
			.unwrap_or(CallbackOutcome::Cancelled)
	}

	/// Deliver the completion page for a claimed code before [`Self::wait`]
	/// resolves the flow.
	pub fn complete_with(&self, success: bool, html: String) {
		let status = if success { 200 } else { 502 };
		let _ = self.control.send(ThreadCommand::Page { status, html });
	}

	/// Stop waiting for the browser unless a callback was already claimed.
	pub fn cancel(&self) {
		let _ = self.control.send(ThreadCommand::Cancel);
	}

	/// Whether a browser callback has been claimed.
	pub fn claimed(&self) -> bool {
		self.state.claimed.load(Ordering::SeqCst)
	}

	/// Shut the listener down.
	pub fn close(&self) {
		self.state.shutdown.store(true, Ordering::SeqCst);
		let _ = self.control.send(ThreadCommand::Close);
	}
}

pub struct CallbackServerOptions {
	/// Provider name used on the browser page, e.g. `OpenAI`.
	pub provider_name: String,
	/// Bind address, e.g. `127.0.0.1`.
	pub host: String,
	/// Bind port; `0` picks a free port.
	pub port: u16,
	/// Callback route, e.g. `/callback`.
	pub path: String,
	/// Host in `redirect_uri` when it differs from `host`.
	pub redirect_host: Option<String>,
	/// Expected `state` query value; `None` skips the check.
	pub state: Option<String>,
	/// Login cancellation signal.
	pub signal: tokio_util::sync::CancellationToken,
	/// Sign-in timeout in milliseconds.
	pub timeout_ms: Option<u64>,
}

enum ConnectionResult {
	/// Immediate error page (wrong route/state/error param/already claimed).
	Rejected,
	/// The parked socket stays open for the flow's completion page.
	Deferred,
}

fn handle_connection(
	stream: TcpStream,
	path: &str,
	expected_state: Option<&str>,
	provider_name: &str,
	state: &ServerState,
	events: &std::sync::mpsc::Sender<CallbackOutcome>,
) -> ConnectionResult {
	let mut reader = BufReader::new(match stream.try_clone() {
		Ok(clone) => clone,
		Err(_) => {
			write_page(stream, 500, &oauth_error_html("Connection failed.", None));
			return ConnectionResult::Rejected;
		}
	});
	let mut request_line = String::new();
	if reader.read_line(&mut request_line).is_err() {
		write_page(stream, 400, &oauth_error_html("Malformed request.", None));
		return ConnectionResult::Rejected;
	}
	// Drain headers (bounded).
	let mut total = 0usize;
	loop {
		let mut line = String::new();
		match reader.read_line(&mut line) {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				total += n;
				if total > 16 * 1024 || line == "\r\n" || line == "\n" {
					break;
				}
			}
		}
	}
	let target = request_line.split_whitespace().nth(1).unwrap_or("/");
	let (route, query) = match target.split_once('?') {
		Some((route, query)) => (route, query),
		None => (target, ""),
	};
	let params = parse_query(query);

	if route != path {
		write_page(
			stream,
			404,
			&oauth_error_html("Callback route not found.", None),
		);
		return ConnectionResult::Rejected;
	}
	if let Some(expected) = expected_state
		&& params.iter().any(|(k, v)| k == "state" && v != expected)
	{
		write_page(stream, 400, &oauth_error_html("State mismatch.", None));
		return ConnectionResult::Rejected;
	}
	if state.claimed.load(Ordering::SeqCst) {
		write_page(
			stream,
			409,
			&oauth_error_html("This sign-in has already been handled.", None),
		);
		return ConnectionResult::Rejected;
	}
	if let Some((_, error)) = params.iter().find(|(k, _)| k == "error") {
		let description = params
			.iter()
			.find(|(k, _)| k == "error_description")
			.map(|(_, v)| v.as_str())
			.unwrap_or(error)
			.to_string();
		write_page(
			stream,
			400,
			&oauth_error_html(
				&format!("{provider_name} authorization failed."),
				Some(&description),
			),
		);
		return ConnectionResult::Rejected;
	}
	let Some((_, code)) = params.iter().find(|(k, _)| k == "code") else {
		// Missing code: 400 and keep serving (upstream continues).
		write_page(
			stream,
			400,
			&oauth_error_html("Missing authorization code.", None),
		);
		return ConnectionResult::Rejected;
	};
	state.claimed.store(true, Ordering::SeqCst);
	// Park the socket so the completion page lands on the same connection.
	*state.parked.lock().unwrap() = Some(stream);
	let _ = events.send(CallbackOutcome::Code(code.clone()));
	ConnectionResult::Deferred
}

/// Serve one page and close the connection.
pub fn write_page(mut stream: TcpStream, status: u16, html: &str) {
	let reason = match status {
		200 => "OK",
		400 => "Bad Request",
		404 => "Not Found",
		409 => "Conflict",
		502 => "Bad Gateway",
		_ => "OK",
	};
	let response = format!(
		"HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
		html.len()
	);
	let _ = stream.write_all(response.as_bytes());
	let _ = stream.flush();
	let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Minimal `application/x-www-form-urlencoded` query parser (values are
/// percent-decoded; `+` is a space).
pub fn parse_query(query: &str) -> Vec<(String, String)> {
	query
		.split('&')
		.filter(|part| !part.is_empty())
		.map(|part| {
			let (key, value) = part.split_once('=').unwrap_or((part, ""));
			(percent_decode(key), percent_decode(value))
		})
		.collect()
}

pub fn percent_decode(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		match bytes[index] {
			b'+' => {
				out.push(b' ');
				index += 1;
			}
			b'%' if index + 2 < bytes.len() => {
				let hex = &text[index + 1..index + 3];
				match u8::from_str_radix(hex, 16) {
					Ok(byte) => {
						out.push(byte);
						index += 3;
					}
					Err(_) => {
						out.push(b'%');
						index += 1;
					}
				}
			}
			byte => {
				out.push(byte);
				index += 1;
			}
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn page_rendering_matches_upstream_shape() {
		let success = oauth_success_html("Signed in to Anthropic. You may now close this page.");
		assert!(success.contains("<title>Authentication successful</title>"));
		assert!(success.contains("Signed in to Anthropic."));
		assert!(success.contains("F09082"), "logo svg present");
		let error = oauth_error_html("Anthropic authorization failed.", Some("bad code"));
		assert!(error.contains("<title>Authentication failed</title>"));
		assert!(error.contains(r#"<div class="details">bad code</div>"#));
		// Escaping: injected markup stays inert.
		let escaped = oauth_error_html("<script>", None);
		assert!(escaped.contains("&lt;script&gt;"));
		assert!(!escaped.contains("<script>"));
	}

	#[test]
	fn query_parser_decodes_form_values() {
		assert_eq!(
			parse_query(
				"code=abc%20def&state=x%2By&error=access_denied&error_description=User+cancelled"
			),
			vec![
				("code".to_string(), "abc def".to_string()),
				("state".to_string(), "x+y".to_string()),
				("error".to_string(), "access_denied".to_string()),
				(
					"error_description".to_string(),
					"User cancelled".to_string()
				),
			]
		);
		// A bare percent stays literal.
		assert_eq!(percent_decode("100%"), "100%");
	}

	#[test]
	fn callback_server_round_trip_success_and_error_pages() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		runtime.block_on(async {
			// Success path: claim, deliver the completion page, fetch it.
			let server = OAuthCallbackServer::start(CallbackServerOptions {
				provider_name: "Anthropic".to_string(),
				host: "127.0.0.1".to_string(),
				port: 0,
				path: "/callback".to_string(),
				redirect_host: Some("localhost".to_string()),
				state: Some("st".to_string()),
				signal: tokio_util::sync::CancellationToken::new(),
				timeout_ms: None,
			})
			.unwrap();
			assert!(server.redirect_uri.starts_with("http://localhost:"));
			let redirect = server.redirect_uri.clone();
			let wait = tokio::spawn({
				let server = std::sync::Arc::new(server);
				let handle = std::sync::Arc::clone(&server);
				async move {
					let outcome = handle.wait().await;
					(outcome, std::sync::Arc::clone(&handle))
				}
			});
			// Give the accept loop a beat, then act like the browser. The
			// browser request stays in flight until `complete_with` delivers
			// the completion page (claimed sockets are parked), so it must
			// run concurrently with the wait.
			tokio::time::sleep(Duration::from_millis(50)).await;
			let browser = tokio::spawn(reqwest::get(format!("{redirect}?code=the-code&state=st")));
			let outcome = wait.await.unwrap();
			assert_eq!(outcome.0, CallbackOutcome::Code("the-code".to_string()));
			outcome
				.1
				.complete_with(true, oauth_success_html("Signed in to Anthropic."));
			let response = browser.await.unwrap().unwrap();
			assert_eq!(response.status(), 200);
			let body = response.text().await.unwrap();
			assert!(body.contains("Authentication successful"));
			assert!(outcome.1.claimed());

			// Error path: a fresh server answers the error page immediately.
			let server = OAuthCallbackServer::start(CallbackServerOptions {
				provider_name: "OpenAI".to_string(),
				host: "127.0.0.1".to_string(),
				port: 0,
				path: "/callback".to_string(),
				redirect_host: None,
				state: None,
				signal: tokio_util::sync::CancellationToken::new(),
				timeout_ms: None,
			})
			.unwrap();
			let url = format!(
				"http://127.0.0.1:{}/callback?error=access_denied&error_description=Nope",
				server.port()
			);
			let response = reqwest::get(url).await.unwrap();
			assert_eq!(response.status(), 400);
			assert!(
				response
					.text()
					.await
					.unwrap()
					.contains("OpenAI authorization failed.")
			);
			server.close();
		});
	}
}
