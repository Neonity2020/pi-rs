//! stdio transport — port of `pi/packages/mcp/src/transports/stdio.ts`.
//!
//! Spawns the MCP server as a child process (`command` + `args` + `env`),
//! frames JSON-RPC one message per newline on the child's stdin/stdout, and
//! continuously drains stderr so a chatty server cannot block. The child runs
//! as the leader of its own process group (Unix), so closing the transport or
//! dropping it terminates the whole tree (`npx`/`uvx` wrappers included) — the
//! same process-group approach as `crates/pi-env/src/local.rs`.
//!
//! Event mapping (mirrors the upstream `emitMessage`/`emitError`/`emitClose`):
//! - parseable stdout line → `TransportEvent::Message`
//! - malformed frame / oversized line / spawn failure → `TransportEvent::Error`
//!   (recoverable; the pump keeps reading, like upstream `emitError`)
//! - child exit or closed pipes → `TransportEvent::Closed` exactly once.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use super::{Transport, TransportEvent};
use crate::protocol::jsonrpc::JsonRpcRequest;

/// Cap on one JSON-RPC line — mirrors upstream `DEFAULT_MAX_MESSAGE_BYTES`.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Tail of stderr kept for diagnostics — mirrors upstream `DEFAULT_MAX_STDERR_BYTES`.
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// How long the server gets to exit on its own after stdin closes, before
/// SIGTERM — mirrors upstream `STDIN_CLOSE_GRACE_MS`.
const STDIN_CLOSE_GRACE_MS: u64 = 500;
/// How long the server gets after SIGTERM before SIGKILL — mirrors upstream
/// `DEFAULT_CLOSE_TIMEOUT_MS`.
const CLOSE_TIMEOUT_MS: u64 = 2_000;

/// Commands from the transport to its supervisor task.
enum SupervisorCommand {
	/// Close requested: the stdin pipe is already dropped; run the spec
	/// shutdown (grace → SIGTERM → SIGKILL) and finish with `Closed`.
	Close,
}

/// Unix: handle for the child's process group; killing the negative pid hits
/// the whole group, so wrappers like `npx` cannot leave the server behind.
/// Dropping the guard SIGKILLs the group as a last-resort orphan guard (unless
/// disarmed after the child was reaped).
#[cfg(unix)]
struct ProcessGroup(Option<u32>);

#[cfg(unix)]
impl ProcessGroup {
	fn signal(&self, signal: i32) {
		if let Some(pid) = self.0 {
			unsafe {
				libc::kill(-(pid as libc::pid_t), signal);
			}
		}
	}

	fn disarm(&mut self) {
		self.0 = None;
	}
}

#[cfg(unix)]
impl Drop for ProcessGroup {
	fn drop(&mut self) {
		self.signal(libc::SIGKILL);
	}
}

pub struct StdioTransport {
	command: String,
	args: Vec<String>,
	env: Vec<(String, String)>,
	cwd: Option<String>,
	stdin: Option<ChildStdin>,
	events: mpsc::UnboundedReceiver<TransportEvent>,
	commands: mpsc::UnboundedSender<SupervisorCommand>,
	/// PID of the spawned child, when a child was spawned at all.
	pid: Option<u32>,
	/// Kept for `stderr()`: bounded tail of what the server wrote to stderr.
	stderr_tail: Arc<Mutex<Vec<u8>>>,
	started: bool,
	close_sent: bool,
	/// Spawn failure surfaced through `start()` and again as an `Error` event.
	startup_error: Option<String>,
}

impl StdioTransport {
	pub fn new(command: impl Into<String>, args: Vec<String>, env: Vec<(String, String)>) -> Self {
		let (commands, _) = mpsc::unbounded_channel();
		let (_, events) = mpsc::unbounded_channel();
		Self {
			command: command.into(),
			args,
			env,
			cwd: None,
			stdin: None,
			events,
			commands,
			pid: None,
			stderr_tail: Arc::new(Mutex::new(Vec::new())),
			started: false,
			close_sent: false,
			startup_error: None,
		}
	}

	/// Optional working directory for the server process.
	pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
		self.cwd = Some(cwd.into());
		self
	}

	/// PID of the spawned server process, if any — mirrors upstream `pid`.
	pub fn pid(&self) -> Option<u32> {
		self.pid
	}

	/// Last `MAX_STDERR_BYTES` of server stderr, for `/mcp` diagnostics —
	/// mirrors upstream `stderr`.
	pub fn stderr(&self) -> String {
		String::from_utf8_lossy(&self.stderr_tail.lock().unwrap()).into_owned()
	}
}

#[async_trait]
impl Transport for StdioTransport {
	async fn start(&mut self) -> crate::Result<()> {
		if self.started {
			return Err(crate::Error::Transport(
				"MCP stdio transport already started".into(),
			));
		}
		self.started = true;

		let mut cmd = Command::new(&self.command);
		cmd.args(&self.args);
		if let Some(cwd) = &self.cwd {
			cmd.current_dir(cwd);
		}
		// Inherit the parent environment and overlay the configured entries,
		// like upstream `inheritEnv: true`.
		for (key, value) in &self.env {
			cmd.env(key, value);
		}
		cmd.stdin(std::process::Stdio::piped());
		cmd.stdout(std::process::Stdio::piped());
		cmd.stderr(std::process::Stdio::piped());
		cmd.kill_on_drop(true);
		// Own process group, so closing the transport can terminate the
		// server's children too.
		#[cfg(unix)]
		{
			use std::os::unix::process::CommandExt;
			cmd.as_std_mut().process_group(0);
		}

		let mut child = match cmd.spawn() {
			Ok(child) => child,
			Err(error) => {
				let message = format!(
					"failed to start MCP stdio server `{}{}`: {error}",
					self.command,
					if self.args.is_empty() {
						String::new()
					} else {
						format!(" {}", self.args.join(" "))
					}
				);
				self.startup_error = Some(message.clone());
				return Err(crate::Error::Transport(message));
			}
		};
		let pid = child.id();
		self.pid = pid;
		let stdin = child.stdin.take().expect("stdin is piped");
		let stdout = child.stdout.take().expect("stdout is piped");
		let stderr = child.stderr.take().expect("stderr is piped");

		let (events_tx, events_rx) = mpsc::unbounded_channel();
		let (commands_tx, commands_rx) = mpsc::unbounded_channel();
		self.events = events_rx;
		self.commands = commands_tx;
		self.stdin = Some(stdin);

		let stderr_tail = Arc::clone(&self.stderr_tail);
		let stderr_task = tokio::spawn(pump_stderr(stderr, stderr_tail));
		let stdout_task = tokio::spawn(pump_stdout(stdout, events_tx.clone()));
		tokio::spawn(supervise(
			child,
			pid,
			events_tx,
			commands_rx,
			stdout_task,
			stderr_task,
		));
		Ok(())
	}

	async fn send(&mut self, request: &JsonRpcRequest) -> crate::Result<()> {
		let Some(stdin) = self.stdin.as_mut() else {
			return Err(crate::Error::Closed);
		};
		let mut payload = super::encode_frame(request).into_bytes();
		payload.push(b'\n');
		let outcome = match stdin.write_all(&payload).await {
			Ok(()) => stdin.flush().await,
			Err(error) => Err(error),
		};
		match outcome {
			Ok(()) => Ok(()),
			// The read end is gone: the server exited or closed its pipes.
			Err(error)
				if matches!(
					error.kind(),
					std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
				) =>
			{
				Err(crate::Error::Closed)
			}
			Err(error) => Err(crate::Error::Transport(error.to_string())),
		}
	}

	async fn recv(&mut self) -> crate::Result<Option<TransportEvent>> {
		if let Some(error) = self.startup_error.take() {
			return Ok(Some(TransportEvent::Error(error)));
		}
		Ok(self.events.recv().await)
	}

	async fn close(&mut self) -> crate::Result<()> {
		if self.close_sent {
			return Ok(());
		}
		self.close_sent = true;
		// Spec shutdown, step 1: close stdin and let the server exit on its
		// own; the supervisor escalates to SIGTERM/SIGKILL (Unix process group
		// first, direct child elsewhere) if it does not.
		self.stdin.take();
		let _ = self.commands.send(SupervisorCommand::Close);
		Ok(())
	}

	fn describe(&self) -> String {
		format!("stdio: {} {}", self.command, self.args.join(" "))
	}
}

/// Outcome of one bounded line read from the server's stdout.
enum Line {
	/// A complete line (newline stripped, trailing `\r` removed).
	Some(String),
	/// A line was discarded for exceeding `MAX_MESSAGE_BYTES`.
	Skipped,
	/// End of stream.
	Eof,
	/// End of stream with an unterminated line still buffered.
	Partial(Vec<u8>),
	/// Read failure.
	Io(std::io::Error),
}

/// Read one `\n`-terminated line into `buf`, bounding memory at
/// `max_message_bytes` like upstream `handleStdout`: content of an oversized
/// line is discarded up to and including its terminating newline.
async fn read_bounded_line(
	reader: &mut (impl AsyncRead + Unpin),
	buf: &mut Vec<u8>,
	max_message_bytes: usize,
) -> Line {
	buf.clear();
	let mut overflow = false;
	let mut chunk = [0u8; 8192];
	loop {
		match reader.read(&mut chunk).await {
			Ok(0) => {
				return if overflow {
					Line::Skipped
				} else if buf.is_empty() {
					Line::Eof
				} else {
					Line::Partial(std::mem::take(buf))
				};
			}
			Ok(n) => {
				let available = &chunk[..n];
				match available.iter().position(|&byte| byte == b'\n') {
					Some(newline) => {
						let line = &available[..newline];
						if overflow || buf.len() + line.len() > max_message_bytes {
							return Line::Skipped;
						}
						buf.extend_from_slice(line);
						if buf.last() == Some(&b'\r') {
							buf.pop();
						}
						return Line::Some(String::from_utf8_lossy(buf).into_owned());
					}
					None => {
						overflow = overflow || buf.len() + available.len() > max_message_bytes;
						if !overflow {
							buf.extend_from_slice(available);
						}
					}
				}
			}
			Err(error) => return Line::Io(error),
		}
	}
}

/// Forward the server's stdout to the transport's event channel, one event per
/// line, until EOF or a read error — mirrors upstream `handleStdout`.
async fn pump_stdout(stdout: ChildStdout, events: mpsc::UnboundedSender<TransportEvent>) {
	let mut reader = BufReader::new(stdout);
	let mut buf = Vec::new();
	loop {
		match read_bounded_line(&mut reader, &mut buf, MAX_MESSAGE_BYTES).await {
			Line::Some(line) => {
				if line.trim().is_empty() {
					continue;
				}
				let event = match super::parse_frame(&line) {
					Ok(frame) => TransportEvent::Message(Box::new(frame)),
					Err(error) => TransportEvent::Error(error.to_string()),
				};
				if events.send(event).is_err() {
					break;
				}
			}
			Line::Skipped => {
				if events
					.send(TransportEvent::Error(format!(
						"MCP stdio message exceeds {MAX_MESSAGE_BYTES} bytes"
					)))
					.is_err()
				{
					break;
				}
			}
			Line::Eof => break,
			Line::Partial(partial) => {
				let _ = events.send(TransportEvent::Error(format!(
					"MCP stdio server closed with an incomplete JSON-RPC message: {}",
					String::from_utf8_lossy(&partial)
				)));
				break;
			}
			Line::Io(error) => {
				let _ = events.send(TransportEvent::Error(format!("stdio read error: {error}")));
				break;
			}
		}
	}
}

/// Continuously drain the server's stderr so it cannot block on a full pipe,
/// keeping a bounded tail for diagnostics — mirrors upstream `handleStderr`.
async fn pump_stderr(stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
	let mut stderr = stderr;
	let mut chunk = [0u8; 8192];
	loop {
		match stderr.read(&mut chunk).await {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				let mut tail = tail.lock().unwrap();
				tail.extend_from_slice(&chunk[..n]);
				let excess = tail.len().saturating_sub(MAX_STDERR_BYTES);
				if excess > 0 {
					tail.drain(..excess);
				}
			}
		}
	}
}

/// Own the child until it exits: merge stdout events, run the shutdown dance
/// on `Close`, and finish with exactly one `Closed` event.
async fn supervise(
	mut child: Child,
	pid: Option<u32>,
	events: mpsc::UnboundedSender<TransportEvent>,
	mut commands: mpsc::UnboundedReceiver<SupervisorCommand>,
	stdout_task: tokio::task::JoinHandle<()>,
	stderr_task: tokio::task::JoinHandle<()>,
) {
	// Orphan guard: if this task ends without reaping the child (transport
	// dropped without `close()`), the drop kills the whole process group.
	#[cfg(unix)]
	let mut group = ProcessGroup(pid);

	// Either the server exits on its own (crash or clean close) or a close was
	// requested and we escalate.
	let mut reaped = false;
	let mut close_requested = false;
	tokio::select! {
		status = child.wait() => {
			if status.is_ok() {
				reaped = true;
			}
		}
		command = commands.recv() => {
			match command {
				Some(SupervisorCommand::Close) => {
					close_requested = true;
					// stdin is already closed by `close()`; give the server
					// the grace window, then SIGTERM, then SIGKILL.
					if tokio::time::timeout(
						Duration::from_millis(STDIN_CLOSE_GRACE_MS),
						child.wait(),
					)
					.await
					.is_ok()
					{
						reaped = true;
					} else {
						#[cfg(unix)]
						group.signal(libc::SIGTERM);
						#[cfg(not(unix))]
						let _ = child.start_kill();
						if tokio::time::timeout(
							Duration::from_millis(CLOSE_TIMEOUT_MS),
							child.wait(),
						)
						.await
						.is_ok()
						{
							reaped = true;
						} else {
							#[cfg(unix)]
							group.signal(libc::SIGKILL);
							#[cfg(not(unix))]
							let _ = child.start_kill();
							reaped = child.wait().await.is_ok();
						}
					}
				}
				// Transport dropped: the kill_on_drop / process-group guard
				// cleans up.
				None => {}
			}
		}
	}

	// Flush whatever the server produced before exiting; bound the wait so a
	// grandchild that inherited stdout cannot stall the close.
	let _ = tokio::time::timeout(Duration::from_millis(CLOSE_TIMEOUT_MS), stdout_task).await;
	stderr_task.abort();

	// Like upstream `close()`: once the child exited, SIGTERM the group so
	// descendants that ignored stdin closing do not outlive the transport.
	#[cfg(unix)]
	if close_requested {
		group.signal(libc::SIGTERM);
	}
	// Only stop the orphan guard after a successful reap; otherwise the guard
	// kills the process group when this task ends (e.g. the transport was
	// dropped without `close()`).
	#[cfg(unix)]
	if reaped {
		group.disarm();
	}
	let _ = events.send(TransportEvent::Closed);
	// Drop `events` so a subsequent `recv()` observes the channel closing.
	drop(events);
}

#[cfg(all(test, unix))]
mod tests {
	use super::*;
	use crate::protocol::content::Content;
	use serde_json::{Value, json};

	/// Fake MCP server over `sh -c`: answers initialize, tools/list, and
	/// tools/call with one `echo` tool; announces itself on stderr.
	const FAKE_SERVER: &str = r#"printf 'fake-mcp ready\n' >&2
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo a message","inputSchema":{"type":"object","properties":{"message":{"type":"string"}}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hello from fake server"}],"isError":false}}\n' "$id" ;;
  esac
done
"#;

	fn stdio(script: &str) -> StdioTransport {
		StdioTransport::new("sh", vec!["-c".into(), script.into()], vec![])
	}

	/// Next event, failing if the channel ended without a `Closed` event.
	async fn next_event(transport: &mut StdioTransport) -> TransportEvent {
		match transport.recv().await.unwrap() {
			Some(event) => event,
			None => panic!("event channel ended without a Closed event"),
		}
	}

	/// Drain events until the transport reports `Closed`.
	async fn until_closed(transport: &mut StdioTransport) {
		loop {
			match transport.recv().await.unwrap() {
				Some(TransportEvent::Closed) => return,
				Some(_) => continue,
				None => panic!("event channel ended without a Closed event"),
			}
		}
	}

	#[tokio::test]
	async fn roundtrips_initialize_list_and_call_with_a_real_process() {
		let mut client = crate::client::McpClient::new("fake", Box::new(stdio(FAKE_SERVER)));
		let info = client.initialize("0.1.0").await.unwrap();
		assert_eq!(info.protocol_version, "2025-06-18");
		assert_eq!(info.server_info.name, "fake");

		let tools = client.list_tools().await.unwrap();
		assert_eq!(tools.len(), 1);
		assert_eq!(tools[0].name, "echo");
		assert_eq!(tools[0].qualified_name, "mcp__fake__echo");
		assert_eq!(tools[0].description.as_deref(), Some("Echo a message"));

		let result = client
			.call_tool("echo", json!({ "message": "hi" }))
			.await
			.unwrap();
		assert!(!result.is_error);
		assert_eq!(result.content.len(), 1);
		assert!(
			matches!(&result.content[0], Content::Text { text } if text == "hello from fake server")
		);
		// Dropping the client drops the transport, which terminates the
		// server's process group.
		drop(client);
	}

	#[tokio::test]
	async fn spawn_failure_surfaces_as_transport_error() {
		let mut transport =
			StdioTransport::new("/nonexistent/pi-mcp-missing-server", vec![], vec![]);
		let error = transport.start().await.unwrap_err();
		assert!(matches!(error, crate::Error::Transport(_)));
		let Some(TransportEvent::Error(message)) = transport.recv().await.unwrap() else {
			panic!("expected an Error event after a failed spawn");
		};
		assert!(message.contains("failed to start"));
		assert!(transport.recv().await.unwrap().is_none());
	}

	#[tokio::test]
	async fn malformed_frames_are_reported_and_skipped() {
		const SCRIPT: &str = r#"printf 'definitely not json\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"ping"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;
		let mut transport = stdio(SCRIPT);
		transport.start().await.unwrap();
		transport
			.send(&JsonRpcRequest::request(7, "ping", Value::Null))
			.await
			.unwrap();

		let TransportEvent::Error(message) = next_event(&mut transport).await else {
			panic!("expected an Error event for the malformed frame");
		};
		assert!(message.contains("invalid frame"));
		let TransportEvent::Message(frame) = next_event(&mut transport).await else {
			panic!("expected the valid response after the malformed frame");
		};
		assert_eq!(frame.id.and_then(|id| id.as_u64()), Some(7));
		transport.close().await.unwrap();
		until_closed(&mut transport).await;
	}

	#[tokio::test]
	async fn oversized_lines_are_dropped_and_reading_continues() {
		const SCRIPT: &str = r#"head -c 20000000 /dev/zero | tr '\0' 'x'
printf '\n'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"ping"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;
		let mut transport = stdio(SCRIPT);
		transport.start().await.unwrap();
		transport
			.send(&JsonRpcRequest::request(3, "ping", Value::Null))
			.await
			.unwrap();

		let TransportEvent::Error(message) = next_event(&mut transport).await else {
			panic!("expected an Error event for the oversized line");
		};
		assert!(message.contains("exceeds"));
		let TransportEvent::Message(frame) = next_event(&mut transport).await else {
			panic!("expected the valid response after the oversized line");
		};
		assert_eq!(frame.id.and_then(|id| id.as_u64()), Some(3));
		transport.close().await.unwrap();
		until_closed(&mut transport).await;
	}

	#[tokio::test]
	async fn server_exit_surfaces_closed() {
		const SCRIPT: &str = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"ping"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"; exit 0 ;;
  esac
done
"#;
		let mut transport = stdio(SCRIPT);
		transport.start().await.unwrap();
		transport
			.send(&JsonRpcRequest::request(1, "ping", Value::Null))
			.await
			.unwrap();

		let TransportEvent::Message(frame) = next_event(&mut transport).await else {
			panic!("expected the ping response");
		};
		assert_eq!(frame.id.and_then(|id| id.as_u64()), Some(1));
		// The server exits after answering; the transport must notice.
		until_closed(&mut transport).await;
		// And a subsequent recv observes the channel end, not a hang.
		assert!(transport.recv().await.unwrap().is_none());
	}

	#[tokio::test]
	async fn killed_server_surfaces_closed() {
		let mut transport = stdio("cat");
		transport.start().await.unwrap();
		let pid = transport.pid().expect("child spawned");
		unsafe {
			libc::kill(pid as libc::pid_t, libc::SIGKILL);
		}
		until_closed(&mut transport).await;
	}

	#[tokio::test]
	async fn close_terminates_the_process_group() {
		// A descendant that ignores stdin closing (the `sleep`) must not
		// outlive the transport once close() ran.
		let marker = std::env::temp_dir().join(format!("pi-mcp-group-{}", std::process::id()));
		let _ = std::fs::remove_file(&marker);
		let script = format!(
			r#"(sleep 30; touch '{}') &
while IFS= read -r line; do :; done
"#,
			marker.display()
		);
		let mut transport = stdio(&script);
		transport.start().await.unwrap();
		transport.close().await.unwrap();
		until_closed(&mut transport).await;
		tokio::time::sleep(Duration::from_millis(200)).await;
		assert!(!marker.exists(), "the server's descendant survived close()");
		let _ = std::fs::remove_file(&marker);
	}

	#[tokio::test]
	async fn stderr_is_drained_and_kept_as_a_bounded_tail() {
		const SCRIPT: &str = r#"i=0
while [ "$i" -lt 2000 ]; do
  printf 'noise line %s\n' "$i" >&2
  i=$((i + 1))
done
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *'"method":"ping"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#;
		let mut transport = stdio(SCRIPT);
		transport.start().await.unwrap();
		// The request roundtrips even while the server floods stderr.
		transport
			.send(&JsonRpcRequest::request(9, "ping", Value::Null))
			.await
			.unwrap();
		let TransportEvent::Message(frame) = next_event(&mut transport).await else {
			panic!("expected the ping response despite stderr noise");
		};
		assert_eq!(frame.id.and_then(|id| id.as_u64()), Some(9));

		let stderr = transport.stderr();
		assert!(
			stderr.contains("noise line 1999"),
			"tail should hold the latest stderr"
		);
		assert!(stderr.len() <= MAX_STDERR_BYTES + 32);
		transport.close().await.unwrap();
		until_closed(&mut transport).await;
	}

	#[tokio::test]
	async fn send_after_close_reports_closed() {
		let mut transport = stdio(FAKE_SERVER);
		transport.start().await.unwrap();
		transport.close().await.unwrap();
		let error = transport
			.send(&JsonRpcRequest::request(1, "ping", Value::Null))
			.await
			.unwrap_err();
		assert!(matches!(error, crate::Error::Closed));
		until_closed(&mut transport).await;
	}
}
