//! Extension-host PoC process bridge — the Rust side of the experiment
//! described in `docs/operations/extensions-host-experiment.md`.
//!
//! Spawns a zero-dependency Node host (`host/host.mjs`, embedded via
//! `include_str!`) that loads one upstream extension file and forwards its
//! `ExtensionAPI` calls over JSONL on stdio. This is a PoC: the API surface
//! and the success criteria it validates are listed in the experiment doc,
//! and nothing here claims upstream compatibility until the criteria pass.
//!
//! Protocol (see host.mjs header): frames are JSONL. The Rust side drives
//! `event`/`call` and answers the host's `call` frames (dialog round trips)
//! via [`ExtensionHost::answer`]. All request/response interleavings run on
//! shared state, so a `call` future can await while a `next_host_call` is
//! being answered.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

const HOST_SCRIPT: &str = include_str!("../host/host.mjs");
const HOOKS_SCRIPT: &str = include_str!("../host/hooks.mjs");

/// Node >= 22.6 ships `--experimental-strip-types`; older runtimes cannot
/// load the TypeScript fixtures.
const MIN_NODE_MAJOR: u32 = 22;
const MIN_NODE_MINOR: u32 = 6;

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, String>>>;

#[derive(Debug)]
pub struct SpawnError {
	pub stage: String,
	pub message: String,
}

impl std::fmt::Display for SpawnError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}: {}", self.stage, self.message)
	}
}

#[derive(Debug, Clone)]
pub struct HostConfig {
	/// Runtime executable ("node" or an absolute path).
	pub runtime: String,
	/// Extension entry file (TS or JS).
	pub extension: PathBuf,
	/// `pi.getFlag` values surfaced to the extension (JSON object).
	pub flags: Value,
	/// Working directory reported through the extension context.
	pub cwd: PathBuf,
	/// Whether dialog-capable UI is available (`ctx.hasUI`).
	pub has_ui: bool,
}

impl Default for HostConfig {
	fn default() -> Self {
		Self {
			runtime: "node".into(),
			extension: PathBuf::new(),
			flags: Value::Null,
			cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
			has_ui: true,
		}
	}
}

/// A registration announcement from the factory phase.
#[derive(Debug, Clone, Deserialize)]
pub struct Registration {
	pub capability: String,
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub event: Option<String>,
	#[serde(default)]
	pub api: Option<String>,
	#[serde(default)]
	pub definition: Option<Value>,
}

/// An unanswered call from the host (dialog round trips, session mutations).
#[derive(Debug, Clone)]
pub struct HostCall {
	pub id: u64,
	pub method: String,
	pub params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WireFrame {
	Ready,
	Register {
		#[serde(flatten)]
		registration: Registration,
	},
	Error {
		stage: String,
		message: String,
	},
	Return {
		id: u64,
		#[serde(default = "ok_default")]
		ok: bool,
		#[serde(default)]
		result: Option<Value>,
		#[serde(default)]
		error: Option<String>,
	},
	Call {
		id: u64,
		method: String,
		#[serde(default)]
		params: Value,
	},
	Action {
		method: String,
		#[serde(default)]
		params: Value,
	},
}

fn ok_default() -> bool {
	true
}

enum Frame {
	Ready,
	Register(Registration),
	Error { stage: String, message: String },
}

fn runtime_probe(runtime: &str) -> Result<(u32, u32), SpawnError> {
	let output = std::process::Command::new(runtime)
		.arg("-e")
		.arg("process.stdout.write(String(process.versions.node))")
		.output()
		.map_err(|e| SpawnError {
			stage: "runtime".into(),
			message: format!("cannot execute {runtime}: {e}"),
		})?;
	let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
	let mut parts = text.split('.');
	let major = parts.next().and_then(|p| p.parse::<u32>().ok());
	let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
	match (major, minor) {
		(Some(major), Some(minor)) => Ok((major, minor)),
		_ => Err(SpawnError {
			stage: "runtime".into(),
			message: format!("cannot parse node version from {text:?}"),
		}),
	}
}

static WORKDIR_SEQ: AtomicU64 = AtomicU64::new(1);

fn materialize_scripts() -> Result<PathBuf, SpawnError> {
	let dir = std::env::temp_dir().join(format!(
		"pi-ext-host-{}-{}",
		std::process::id(),
		WORKDIR_SEQ.fetch_add(1, Ordering::Relaxed)
	));
	std::fs::create_dir_all(&dir).map_err(|e| SpawnError {
		stage: "workdir".into(),
		message: format!("cannot create {}: {e}", dir.display()),
	})?;
	std::fs::write(dir.join("host.mjs"), HOST_SCRIPT).map_err(|e| SpawnError {
		stage: "workdir".into(),
		message: format!("cannot write host.mjs: {e}"),
	})?;
	std::fs::write(dir.join("hooks.mjs"), HOOKS_SCRIPT).map_err(|e| SpawnError {
		stage: "workdir".into(),
		message: format!("cannot write hooks.mjs: {e}"),
	})?;
	Ok(dir)
}

struct Inner {
	child: Mutex<Child>,
	stdin: Mutex<tokio::process::ChildStdin>,
	pending: Arc<Mutex<PendingMap>>,
	host_calls: Mutex<mpsc::Receiver<HostCall>>,
	registrations: Mutex<Vec<Registration>>,
	stderr_tail: Arc<Mutex<Vec<u8>>>,
	actions: Arc<Mutex<Vec<HostCall>>>,
	workdir: PathBuf,
}

/// A running extension host process. Cloneable: the connection state is
/// shared, so request futures never borrow the handle.
#[derive(Clone)]
pub struct ExtensionHost {
	inner: Arc<Inner>,
	next_id: Arc<AtomicU64>,
}

impl ExtensionHost {
	/// Spawn the host and wait for the factory phase to complete (`ready`).
	/// Retries once with `--experimental-strip-types` for Node versions that
	/// ship type stripping behind the flag.
	pub async fn spawn(config: HostConfig) -> Result<Self, SpawnError> {
		let (major, minor) = runtime_probe(&config.runtime)?;
		if (major, minor) < (MIN_NODE_MAJOR, MIN_NODE_MINOR) {
			return Err(SpawnError {
				stage: "runtime".into(),
				message: format!(
					"node {major}.{minor} is too old for TS type stripping (need >= {MIN_NODE_MAJOR}.{MIN_NODE_MINOR})"
				),
			});
		}
		match Self::spawn_once(&config, false).await {
			Err(err) if err.stage == "import" => Self::spawn_once(&config, true).await,
			other => other,
		}
	}

	async fn spawn_once(config: &HostConfig, strip_types_flag: bool) -> Result<Self, SpawnError> {
		let workdir = materialize_scripts()?;
		let mut command = Command::new(&config.runtime);
		if strip_types_flag {
			command.arg("--experimental-strip-types");
		}
		command
			.arg("--import")
			.arg(workdir.join("hooks.mjs"))
			.arg(workdir.join("host.mjs"))
			.arg(&config.extension)
			.env("PI_EXT_FLAGS", config.flags.to_string())
			.env("PI_EXT_CWD", &config.cwd)
			.env("PI_EXT_HAS_UI", if config.has_ui { "1" } else { "0" })
			.current_dir(&config.cwd)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true);
		let mut child = command.spawn().map_err(|e| SpawnError {
			stage: "spawn".into(),
			message: format!("cannot spawn {}: {e}", config.runtime),
		})?;

		let stdin = child.stdin.take().ok_or(SpawnError {
			stage: "spawn".into(),
			message: "missing stdin".into(),
		})?;
		let stdout = child.stdout.take().ok_or(SpawnError {
			stage: "spawn".into(),
			message: "missing stdout".into(),
		})?;
		let mut stderr = child.stderr.take().ok_or(SpawnError {
			stage: "spawn".into(),
			message: "missing stderr".into(),
		})?;

		let (control_tx, mut control_rx) = mpsc::channel(64);
		let (host_calls_tx, host_calls_rx) = mpsc::channel(64);

		let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
		let stderr_tail = Arc::new(Mutex::new(Vec::new()));
		let actions: Arc<Mutex<Vec<HostCall>>> = Arc::new(Mutex::new(Vec::new()));

		// Frame pump: control frames to the factory loop, returns to their
		// waiters, host-initiated calls to the answer queue.
		let state_pending = pending.clone();
		let actions_for_pump = actions.clone();
		tokio::spawn(async move {
			let mut lines = BufReader::new(stdout).lines();
			while let Ok(Some(line)) = lines.next_line().await {
				let Ok(frame) = serde_json::from_str::<WireFrame>(&line) else {
					continue;
				};
				match frame {
					WireFrame::Ready => {
						let _ = control_tx.send(Frame::Ready).await;
					}
					WireFrame::Register { registration } => {
						let _ = control_tx.send(Frame::Register(registration)).await;
					}
					WireFrame::Error { stage, message } => {
						let _ = control_tx.send(Frame::Error { stage, message }).await;
					}
					WireFrame::Return {
						id,
						ok,
						result,
						error,
					} => {
						if let Some(sender) = state_pending.lock().await.remove(&id) {
							let _ = sender.send(match (ok, error) {
								(true, _) => Ok(result.unwrap_or(Value::Null)),
								(false, Some(err)) => Err(err),
								(false, None) => Err("host call failed".into()),
							});
						}
					}
					WireFrame::Call { id, method, params } => {
						let _ = host_calls_tx.send(HostCall { id, method, params }).await;
					}
					WireFrame::Action { method, params } => {
						actions_for_pump.lock().await.push(HostCall {
							id: 0,
							method,
							params,
						});
					}
				}
			}
		});

		// Stderr tail: extension stdout relay plus diagnostics, capped. Raw
		// bytes, not lines — extension terminal escapes (OSC 777 etc.) have
		// no trailing newline.
		{
			let tail = stderr_tail.clone();
			tokio::spawn(async move {
				use tokio::io::AsyncReadExt;
				let mut buf = [0u8; 4096];
				loop {
					match stderr.read(&mut buf).await {
						Ok(0) | Err(_) => break,
						Ok(n) => {
							let mut guard = tail.lock().await;
							guard.extend_from_slice(&buf[..n]);
							if guard.len() > 64 * 1024 {
								let cut = guard.len() - 32 * 1024;
								guard.drain(..cut);
							}
						}
					}
				}
			});
		}

		let inner = Arc::new(Inner {
			child: Mutex::new(child),
			stdin: Mutex::new(stdin),
			pending,
			host_calls: Mutex::new(host_calls_rx),
			registrations: Mutex::new(Vec::new()),
			stderr_tail,
			actions,
			workdir,
		});
		let host = Self {
			inner,
			next_id: Arc::new(AtomicU64::new(1)),
		};

		// Factory phase: collect registrations until ready (or a hard error).
		let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
		loop {
			let frame = match tokio::time::timeout_at(deadline, control_rx.recv()).await {
				Ok(frame) => frame,
				Err(_) => {
					return Err(SpawnError {
						stage: "factory".into(),
						message: format!(
							"timed out waiting for the extension factory; stderr tail: {}",
							host.stderr_tail().await
						),
					});
				}
			};
			match frame {
				Some(Frame::Ready) => break,
				Some(Frame::Register(registration)) => {
					host.inner.registrations.lock().await.push(registration);
				}
				Some(Frame::Error { stage, message }) => {
					let _ = host.inner.child.lock().await.kill().await;
					return Err(SpawnError { stage, message });
				}
				None => {
					return Err(SpawnError {
						stage: "factory".into(),
						message: format!(
							"host exited before ready; stderr tail: {}",
							host.stderr_tail().await
						),
					});
				}
			}
		}
		Ok(host)
	}

	async fn send(&self, payload: String) -> Result<(), String> {
		let mut stdin = self.inner.stdin.lock().await;
		stdin
			.write_all(payload.as_bytes())
			.await
			.map_err(|e| format!("cannot write to host: {e}"))?;
		stdin
			.flush()
			.await
			.map_err(|e| format!("cannot flush host stdin: {e}"))
	}

	fn alloc_id(&self) -> u64 {
		self.next_id.fetch_add(1, Ordering::Relaxed)
	}

	/// Tail (last ~2KB) of the host's stderr — extension stdout relay plus
	/// diagnostics; assertions on extension terminal output read this.
	pub async fn stderr_tail(&self) -> String {
		let guard = self.inner.stderr_tail.lock().await;
		String::from_utf8_lossy(guard.as_slice())
			.chars()
			.rev()
			.take(2000)
			.collect::<Vec<_>>()
			.into_iter()
			.rev()
			.collect()
	}

	/// One-way `action` frames from the host (sync-API setters, appendEntry).
	pub async fn actions(&self) -> Vec<HostCall> {
		self.inner.actions.lock().await.clone()
	}

	/// Snapshot the factory-phase registrations.
	pub async fn registrations(&self) -> Vec<Registration> {
		self.inner.registrations.lock().await.clone()
	}

	/// Dispatch an event to every handler registered for `event` and collect
	/// the per-handler outcomes (`null` where a handler returned nothing).
	pub async fn dispatch_event(
		&self,
		event: &str,
		payload: Value,
		timeout: Duration,
	) -> Result<Vec<Value>, String> {
		let result = self
			.request(
				serde_json::json!({
					"type": "event",
					"event": event,
					"payload": payload,
				}),
				timeout,
			)
			.await?;
		Ok(result.as_array().cloned().unwrap_or_default())
	}

	/// Call a host method (`tool.execute`, `command.execute`) and await its result.
	pub async fn call(
		&self,
		method: &str,
		params: Value,
		timeout: Duration,
	) -> Result<Value, String> {
		self.request(
			serde_json::json!({
				"type": "call",
				"method": method,
				"params": params,
			}),
			timeout,
		)
		.await
	}

	async fn request(&self, mut frame: Value, timeout: Duration) -> Result<Value, String> {
		let id = self.alloc_id();
		frame["id"] = serde_json::json!(id);
		let (tx, rx) = oneshot::channel();
		self.inner.pending.lock().await.insert(id, tx);
		self.send(format!("{frame}\n")).await?;
		match tokio::time::timeout(timeout, rx).await {
			Ok(Ok(outcome)) => outcome,
			Ok(Err(_)) => Err("host dropped the return channel".into()),
			Err(_) => Err(format!("request {id} timed out")),
		}
	}

	/// Receive the next host-initiated call (dialog round trips and friends).
	pub async fn next_host_call(&self, timeout: Duration) -> Option<HostCall> {
		let mut receiver = self.inner.host_calls.lock().await;
		tokio::time::timeout(timeout, receiver.recv())
			.await
			.ok()
			.flatten()
	}

	/// Answer a host-initiated call.
	pub async fn answer(&self, id: u64, result: Value) -> Result<(), String> {
		let frame = serde_json::json!({ "type": "return", "id": id, "ok": true, "result": result });
		self.send(format!("{frame}\n")).await
	}

	/// Graceful shutdown: ask, wait, then kill; remove the extracted scripts.
	pub async fn shutdown(&self) {
		let _ = self
			.send(format!("{}\n", serde_json::json!({"type": "shutdown"})))
			.await;
		let mut child = self.inner.child.lock().await;
		let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
		let _ = child.kill().await;
		let _ = std::fs::remove_dir_all(&self.inner.workdir);
	}

	pub fn workdir(&self) -> PathBuf {
		self.inner.workdir.clone()
	}
}
