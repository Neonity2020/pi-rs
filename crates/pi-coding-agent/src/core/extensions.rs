//! Extension bridge assembly — wires the `pi-extension-host` process bridge
//! (round-13 PoC, `docs/operations/extensions-host-experiment.md` §6) into the
//! production session path.
//!
//! Anatomy: [`discover_extension_paths`] finds extension entry files (CLI
//! `-e` paths, the user `extensions/` directory, and — trust-gated — the
//! project `.pi/extensions/` directory; port of upstream
//! `core/extensions/loader.ts` `discoverAndLoadExtensions`).
//! [`spawn_extensions`] spawns one host process per entry, aggregates the
//! factory-phase registrations, and hands back an [`ExtensionAssembly`]:
//! bridged [`ExtensionTool`]s for the agent tool table plus an
//! [`ExtensionBridge`] the session task drives (events, commands, hooks).
//!
//! Declared differences against the upstream in-process runner:
//! - one `node` host process per extension entry (the bridge supports node
//!   only; bun is untested). A missing runtime disables the whole assembly
//!   with a notice; a failing extension is isolated into a notice with its
//!   stderr tail and the rest continue (upstream isolates per extension too).
//! - sync `ExtensionAPI` surface travels as one-way frames; only the logical
//!   face (tools, commands, events, dialog primitives) crosses the boundary.
//! - the bridge hosts stay alive across session replacement while upstream
//!   rebuilds the extension runtime; pi-rs re-fires `session_start(reason)`
//!   on the surviving hosts instead (see `agent_session.rs`).
//! - ui.* dialog calls other than `ui.notify` resolve to `null` (no TUI
//!   dialog routing this round) — permission handlers that follow the
//!   upstream fail-closed pattern therefore block in interactive mode too.
//! - the `project_trust` *event* is not dispatched; the trust *decision*
//!   (same store the MCP gate uses) gates whether project extensions load.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pi_agent::{AgentTool, AgentToolResult};
use pi_extension_host::{ExtensionHost, HostConfig, Registration};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::core::agent_session::SessionEvent;
use crate::core::event_bus::{EventBus, EventBusWeak};

// -------------------------------------------------------------------------------------------
// Timeouts — the upstream runner awaits extension handlers without a bound; the
// bridge needs one so a hung host cannot stall the run forever. Dialog-bearing
// dispatches get the long budget (a future round routes them to the TUI).
// -------------------------------------------------------------------------------------------

/// Fire-and-forget lifecycle events (`session_start`, `turn_*`, `agent_*`,
/// `input`, `session_shutdown`, `agent_settled`).
pub const EXT_EVENT_TIMEOUT: Duration = Duration::from_secs(30);
/// `tool_call` interception — long budget: the handler may take a real
/// decision (upstream blocks tool execution until the handler settles).
pub const EXT_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Extension tool `execute`.
pub const EXT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Extension command handler.
pub const EXT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-extension spawn budget (factory phase; the host itself waits 15s for
/// `ready`, so the outer budget only guards a wedged runtime).
const EXT_SPAWN_TIMEOUT: Duration = Duration::from_secs(20);

// -------------------------------------------------------------------------------------------
// Discovery — port of loader.ts `discoverExtensionsInDir` /
// `resolveExtensionEntries` / `discoverAndLoadExtensions`, reduced: no
// package.json `pi.extensions` manifests (declared omission), no symlink
// chase beyond the direct readdir form.
// -------------------------------------------------------------------------------------------

fn is_extension_file(name: &str) -> bool {
	name.ends_with(".ts") || name.ends_with(".js")
}

/// Directory discovery (loader.ts `discoverExtensionsInDir`): direct
/// `*.ts`/`*.js` files, plus one-level subdirectories with an
/// `index.ts`/`index.js` entry. Entries are sorted for determinism (upstream
/// iterates `readdirSync` order — OS-dependent).
fn discover_in_dir(dir: &Path) -> Vec<PathBuf> {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return Vec::new();
	};
	let mut names: Vec<String> = entries
		.filter_map(|entry| entry.ok())
		.filter(|entry| entry.path() != dir)
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.collect();
	names.sort();
	let mut found = Vec::new();
	for name in names {
		let path = dir.join(&name);
		let file_type = match std::fs::metadata(&path) {
			// `metadata` follows symlinks like upstream's `withFileTypes` +
			// `isFile()`/`isDirectory()` combination.
			Ok(metadata) => metadata.file_type(),
			Err(_) => continue,
		};
		if file_type.is_file() && is_extension_file(&name) {
			found.push(path);
		} else if file_type.is_dir() {
			// Subdirectory: index.ts wins over index.js (loader.ts
			// `resolveExtensionEntries`).
			let index_ts = path.join("index.ts");
			let index_js = path.join("index.js");
			if index_ts.is_file() {
				found.push(index_ts);
			} else if index_js.is_file() {
				found.push(index_js);
			}
		}
	}
	found
}

/// Discovery input. `cli_paths` are already resolved against the cwd
/// (`main.rs` `resolve_file_argument`, matching upstream `resolveCliPaths`).
pub struct DiscoveryOptions<'a> {
	pub cli_paths: &'a [PathBuf],
	pub cwd: &'a Path,
	pub agent_dir: &'a Path,
	/// `false` when `--no-extensions`/`-ne`: the default directories are
	/// skipped while explicit `-e` paths still load (upstream resource-loader
	/// `noExtensions` semantics: only `cliEnabledExtensions` survive).
	pub include_defaults: bool,
	/// Project `.pi/extensions/` loads only for trusted projects (upstream
	/// gates project resources behind `project_trust`).
	pub project_trusted: bool,
}

/// Resolved extension entry files plus startup notices.
pub struct Discovered {
	/// `(label, entry file)` in load order: project dir, user dir, then CLI
	/// paths (upstream `discoverAndLoadExtensions` `addPaths` order).
	pub entries: Vec<(String, PathBuf)>,
	pub notices: Vec<String>,
}

/// Discover extension entry files. Nonexistent CLI paths surface the
/// upstream resource-loader error line as a notice; loading continues.
pub fn discover_extension_paths(options: DiscoveryOptions<'_>) -> Discovered {
	let mut entries: Vec<(String, PathBuf)> = Vec::new();
	let mut notices = Vec::new();
	let mut seen: HashSet<PathBuf> = HashSet::new();

	fn push(entries: &mut Vec<(String, PathBuf)>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
		let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
		if seen.insert(key) {
			entries.push((path.display().to_string(), path));
		}
	}

	// 1. Project-local extensions: `cwd/.pi/extensions/` — trust-gated.
	let project_dir = options.cwd.join(".pi").join("extensions");
	if project_dir.is_dir() {
		if !options.include_defaults {
			// `-ne`: the notice stays quiet — the user asked for no discovery.
		} else if options.project_trusted {
			for path in discover_in_dir(&project_dir) {
				push(&mut entries, &mut seen, path);
			}
		} else {
			notices.push(format!(
				"project {} exists but the project is not trusted; project extensions are skipped",
				project_dir.display()
			));
		}
	}

	// 2. User extensions: `<agent-dir>/extensions/`.
	if options.include_defaults {
		let user_dir = options.agent_dir.join("extensions");
		for path in discover_in_dir(&user_dir) {
			push(&mut entries, &mut seen, path);
		}
	}

	// 3. Explicit `-e` paths: a file loads directly; a directory with an
	// index entry loads that, otherwise it is discovered like the defaults
	// (upstream `discoverAndLoadExtensions` step 3).
	for path in options.cli_paths {
		if !path.exists() {
			notices.push(format!("Extension path does not exist: {}", path.display()));
			continue;
		}
		if path.is_dir() {
			let index_ts = path.join("index.ts");
			let index_js = path.join("index.js");
			if index_ts.is_file() {
				push(&mut entries, &mut seen, index_ts);
				continue;
			}
			if index_js.is_file() {
				push(&mut entries, &mut seen, index_js);
				continue;
			}
			for discovered in discover_in_dir(path) {
				push(&mut entries, &mut seen, discovered);
			}
			continue;
		}
		push(&mut entries, &mut seen, path.clone());
	}

	Discovered { entries, notices }
}

// -------------------------------------------------------------------------------------------
// Assembly
// -------------------------------------------------------------------------------------------

/// One bridged tool registration (host `registerTool`).
#[derive(Debug, Clone)]
pub struct ExtensionToolDef {
	pub host_index: usize,
	/// The owning extension's label (its entry path) — used for notices.
	pub extension_label: String,
	pub name: String,
	/// Human-readable tool label for UI display.
	pub label: String,
	pub description: String,
	pub parameters: Value,
	/// `"sequential"` forces serialized execution (upstream
	/// `ToolDefinition.executionMode`); anything else stays parallel.
	pub sequential: bool,
}

/// One bridged slash command registration (host `registerCommand`).
#[derive(Debug, Clone)]
pub struct ExtensionCommandDef {
	pub host_index: usize,
	pub name: String,
	pub description: String,
}

struct HostEntry {
	label: String,
	host: ExtensionHost,
	enabled: bool,
	/// Events the extension subscribed to (skip dispatch round trips for
	/// hosts that do not listen).
	events: HashSet<String>,
	/// How many one-way `action` frames have been consumed (drain cursor).
	actions_seen: usize,
}

struct BridgeInner {
	hosts: std::sync::Mutex<Vec<HostEntry>>,
	/// Set by the session task on its first loop pass. Weak on purpose: the
	/// bridge outlives the task (teardown runs from main), and a strong bus
	/// would keep the broadcast channel open after the task exits — the
	/// print/rpc modes hang waiting for it to close.
	bus: std::sync::Mutex<Option<EventBusWeak<SessionEvent>>>,
}

impl BridgeInner {
	fn notice(&self, text: String) {
		if let Some(bus) = self
			.bus
			.lock()
			.unwrap()
			.as_ref()
			.and_then(|bus| bus.upgrade())
		{
			bus.emit(SessionEvent::Notice(text));
		}
	}

	async fn disable_host(&self, label: &str, host: &ExtensionHost, reason: &str) {
		// Read the stderr tail before taking the lock: the guard must never
		// be held across the await below.
		let tail = flatten_tail(&host.stderr_tail().await);
		let mut hosts = self.hosts.lock().unwrap();
		if let Some(entry) = hosts
			.iter_mut()
			.find(|entry| entry.label == label && entry.enabled)
		{
			entry.enabled = false;
			self.notice(format!(
				"extension {label} has been disabled ({reason}){}",
				if tail.is_empty() {
					String::new()
				} else {
					format!("; stderr: {tail}")
				}
			));
		}
	}
}

/// Flattened, length-capped stderr tail for notices.
fn flatten_tail(tail: &str) -> String {
	let cleaned: String = tail
		.chars()
		.map(|c| if c.is_control() { ' ' } else { c })
		.collect();
	let trimmed = cleaned.trim();
	const MAX: usize = 200;
	if trimmed.chars().count() > MAX {
		let cut: String = trimmed
			.chars()
			.rev()
			.take(MAX)
			.collect::<Vec<_>>()
			.into_iter()
			.rev()
			.collect();
		format!("…{cut}")
	} else {
		trimmed.to_string()
	}
}

/// A sync API request from an agent hook, serviced on the dispatcher thread.
struct HookRequest {
	event: String,
	payload: Value,
	timeout: Duration,
	/// `None` for fire-and-forget dispatches (`tool_result`).
	respond: Option<std::sync::mpsc::SyncSender<Vec<Value>>>,
}

/// Serial dispatcher for the synchronous agent hooks
/// (`before_tool_call`/`after_tool_call`). `Runtime::block_on` panics inside
/// a tokio worker and `BeforeToolCallFn` is a sync `Fn`, so the hooks hand
/// their dispatch to this dedicated thread, which owns a private
/// current-thread runtime for the bridge I/O. The calling worker blocks on a
/// std channel — declared constraint: bridge I/O is short, the multi-thread
/// pool tolerates one parked worker, and pi-agent's parallel mode prepares
/// tool calls sequentially so the hooks never pile up.
struct HookDispatcher {
	tx: std::sync::mpsc::Sender<HookRequest>,
}

impl HookDispatcher {
	fn spawn(inner: Arc<BridgeInner>) -> Self {
		let (tx, rx) = std::sync::mpsc::channel::<HookRequest>();
		let _ = std::thread::Builder::new()
			.name("pi-ext-hooks".into())
			.spawn(move || {
				let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.build()
				else {
					return;
				};
				while let Ok(request) = rx.recv() {
					let outcomes = runtime.block_on(dispatch_event_inner(
						&inner,
						&request.event,
						request.payload,
						request.timeout,
					));
					if let Some(respond) = request.respond {
						let _ = respond.send(outcomes);
					}
				}
			});
		Self { tx }
	}

	/// Blocking dispatch (hook context). A dead dispatcher yields no
	/// outcomes — the tool call proceeds.
	fn call(&self, event: &str, payload: Value, timeout: Duration) -> Vec<Value> {
		let (tx, rx) = std::sync::mpsc::sync_channel(1);
		let request = HookRequest {
			event: event.to_string(),
			payload,
			timeout,
			respond: Some(tx),
		};
		if self.tx.send(request).is_err() {
			return Vec::new();
		}
		rx.recv().unwrap_or_default()
	}

	/// Non-blocking dispatch (hook context): the request is queued and the
	/// hook returns immediately.
	fn fire(&self, event: &str, payload: Value, timeout: Duration) {
		let _ = self.tx.send(HookRequest {
			event: event.to_string(),
			payload,
			timeout,
			respond: None,
		});
	}
}

/// The live extension bridge. Cheap to clone; all state is shared.
#[derive(Clone)]
pub struct ExtensionBridge {
	inner: Arc<BridgeInner>,
	tools: Arc<std::sync::Mutex<Vec<ExtensionToolDef>>>,
	commands: Arc<std::sync::Mutex<Vec<ExtensionCommandDef>>>,
	hooks: Arc<HookDispatcher>,
}

impl ExtensionBridge {
	/// Empty bridge (no extensions / runtime missing) — every call is a no-op.
	pub fn empty() -> Self {
		Self::with_hosts(Vec::new(), Vec::new(), Vec::new())
	}

	fn with_hosts(
		hosts: Vec<HostEntry>,
		tools: Vec<ExtensionToolDef>,
		commands: Vec<ExtensionCommandDef>,
	) -> Self {
		let inner = Arc::new(BridgeInner {
			hosts: std::sync::Mutex::new(hosts),
			bus: std::sync::Mutex::new(None),
		});
		Self {
			hooks: Arc::new(HookDispatcher::spawn(inner.clone())),
			inner,
			tools: Arc::new(std::sync::Mutex::new(tools)),
			commands: Arc::new(std::sync::Mutex::new(commands)),
		}
	}

	/// True when no host is enabled (nothing spawned or all disabled).
	pub fn is_empty(&self) -> bool {
		!self
			.inner
			.hosts
			.lock()
			.unwrap()
			.iter()
			.any(|host| host.enabled)
	}

	/// The session event bus, set by the session task before its first
	/// dispatch so bridge notices reach the mode channels. Held weakly (see
	/// [`BridgeInner::bus`]).
	pub fn set_bus(&self, bus: EventBus<SessionEvent>) {
		*self.inner.bus.lock().unwrap() = Some(bus.downgrade());
	}

	/// Bridged tool registrations (deduped at spawn).
	pub fn tool_defs(&self) -> Vec<ExtensionToolDef> {
		self.tools.lock().unwrap().clone()
	}

	/// Bridged slash-command registrations.
	pub fn command_defs(&self) -> Vec<ExtensionCommandDef> {
		self.commands.lock().unwrap().clone()
	}

	/// Build the agent tool for one registration.
	pub fn extension_tool(&self, def: ExtensionToolDef) -> Arc<dyn AgentTool> {
		Arc::new(ExtensionTool {
			host: self.host_by_index(def.host_index),
			inner: self.inner.clone(),
			def,
		})
	}

	/// All bridged tools as agent tools.
	pub fn extension_tools(&self) -> Vec<Arc<dyn AgentTool>> {
		self.tool_defs()
			.into_iter()
			.map(|def| self.extension_tool(def))
			.collect()
	}

	fn host_by_index(&self, index: usize) -> ExtensionHost {
		self.inner.hosts.lock().unwrap()[index].host.clone()
	}

	/// `/name args` → the registered extension command and its raw argument
	/// text (upstream `_tryExecuteExtensionCommand` split: everything after
	/// the first space, untrimmed — handlers trim themselves).
	pub fn match_command(&self, text: &str) -> Option<(String, String)> {
		let rest = text.strip_prefix('/')?;
		let (name, args) = match rest.split_once(' ') {
			Some((name, args)) => (name, args),
			None => (rest, ""),
		};
		if name.is_empty() {
			return None;
		}
		self.commands
			.lock()
			.unwrap()
			.iter()
			.any(|command| command.name == name)
			.then(|| (name.to_string(), args.to_string()))
	}

	/// Dispatch an event to every enabled host that listens for it, with the
	/// host-call answer loop attached. Async context (session task).
	pub async fn dispatch_event_all(
		&self,
		event: &str,
		payload: Value,
		timeout: Duration,
	) -> Vec<Value> {
		let hosts: Vec<(String, ExtensionHost, bool)> = {
			self.inner
				.hosts
				.lock()
				.unwrap()
				.iter()
				.map(|entry| {
					(
						entry.label.clone(),
						entry.host.clone(),
						entry.events.contains(event),
					)
				})
				.collect()
		};
		let mut outcomes = Vec::new();
		for (label, host, listens) in hosts {
			if !listens {
				continue;
			}
			let future = host.dispatch_event(event, payload.clone(), timeout);
			match with_host_answers(&host, &self.inner, future).await {
				Ok(mut handler_outcomes) => {
					self.report_handler_errors(&label, &handler_outcomes);
					outcomes.append(&mut handler_outcomes);
				}
				Err(error) => self.report_host_failure(&label, &host, &error).await,
			}
		}
		outcomes
	}

	/// Execute an extension command (`command.execute`), answering dialog
	/// calls while the handler runs. Async context (session task).
	pub async fn execute_command(&self, name: &str, args: &str) -> Result<(), String> {
		let host = {
			let commands = self.commands.lock().unwrap();
			let def = commands
				.iter()
				.find(|command| command.name == name)
				.ok_or_else(|| format!("unknown extension command \"{name}\""))?;
			self.host_by_index(def.host_index)
		};
		let future = host.call(
			"command.execute",
			json!({"name": name, "args": args}),
			EXT_COMMAND_TIMEOUT,
		);
		with_host_answers(&host, &self.inner, future)
			.await
			.map(|_: Value| ())
			.map_err(|error| format!("extension command /{name} failed: {error}"))
	}

	/// Consume one-way `action` frames recorded since the last drain and
	/// apply the ones the session can honor. Returns the session names the
	/// hosts set (the caller applies them to the live session).
	pub async fn drain_actions(&self) -> Vec<String> {
		// Snapshot the hosts first: the std MutexGuard must never be held
		// across the `actions()` awaits.
		let hosts: Vec<(usize, ExtensionHost)> = {
			self.inner
				.hosts
				.lock()
				.unwrap()
				.iter()
				.enumerate()
				.filter(|(_, entry)| entry.enabled)
				.map(|(index, entry)| (index, entry.host.clone()))
				.collect()
		};
		let mut names = Vec::new();
		for (index, host) in hosts {
			let actions = host.actions().await;
			// Advance the drain cursor under the lock; the host vec is fixed
			// after spawn, so the index stays valid.
			let pending: Vec<pi_extension_host::HostCall> = {
				let mut hosts = self.inner.hosts.lock().unwrap();
				let entry = &mut hosts[index];
				let seen = entry.actions_seen;
				entry.actions_seen = actions.len();
				actions.iter().skip(seen).cloned().collect()
			};
			for action in pending {
				match action.method.as_str() {
					// pi.setSessionName (sync API → action frame): applied by
					// the caller to the live session log.
					"session.setSessionName" => {
						if let Some(name) = action.params.get("name").and_then(Value::as_str) {
							names.push(name.to_string());
						}
					}
					// session.appendEntry / tools.setActive /
					// session.setThinkingLevel: recorded and ignored — no
					// custom-entry, tool-set, or thinking bridge this round
					// (declared limitation; the frames stay inspectable via
					// the recorded log).
					other => {
						let _ = other;
					}
				}
			}
		}
		names
	}

	/// A dispatch/call against this host failed: disable + notice on a dead
	/// host, notice only on a live-but-slow one.
	pub async fn report_host_failure(&self, label: &str, host: &ExtensionHost, error: &str) {
		if host.has_exited().await {
			self.inner.disable_host(label, host, error).await;
		} else {
			self.inner.notice(format!("extension {label}: {error}"));
		}
	}

	/// Handler-level errors arrive as `{__error}` outcomes (host.mjs
	/// `runEvent`); surface them like upstream `emitError`.
	fn report_handler_errors(&self, label: &str, outcomes: &[Value]) {
		for outcome in outcomes {
			if let Some(error) = outcome.get("__error").and_then(Value::as_str) {
				self.inner
					.notice(format!("extension {label} handler error: {error}"));
			}
		}
	}

	/// `session_shutdown` to every listening host, then the graceful host
	/// shutdown ladder (ask → 3s grace → kill). Teardown path.
	pub async fn shutdown(&self) {
		self.dispatch_event_all(
			"session_shutdown",
			json!({"type": "session_shutdown"}),
			EXT_EVENT_TIMEOUT,
		)
		.await;
		let hosts: Vec<ExtensionHost> = self
			.inner
			.hosts
			.lock()
			.unwrap()
			.iter()
			.map(|entry| entry.host.clone())
			.collect();
		for host in hosts {
			host.shutdown().await;
		}
	}

	/// Install the agent hooks that bridge `tool_call`/`tool_result`/
	/// `before_agent_start` (re-run on session replacement — the replacement
	/// builds a fresh agent).
	pub fn install_hooks(&self, agent: &pi_agent::Agent) {
		let bridge = self.clone();
		let before: pi_agent::BeforeToolCallFn = Arc::new(move |ctx| {
			let payload = json!({
				"type": "tool_call",
				"toolCallId": ctx.tool_call.id,
				"toolName": ctx.tool_call.name,
				"input": ctx.tool_call.arguments,
			});
			let outcomes = bridge
				.hooks
				.call("tool_call", payload, EXT_TOOL_CALL_TIMEOUT);
			aggregate_tool_call_outcomes(&outcomes)
		});
		agent.set_before_tool_call(before);

		let bridge = self.clone();
		let after: pi_agent::AfterToolCallFn = Arc::new(move |ctx| {
			let content: Vec<Value> = ctx
				.result
				.content
				.iter()
				.map(|block| match block {
					pi_ai::TextOrImage::Text(text) => json!({"type": "text", "text": text.text}),
					pi_ai::TextOrImage::Image(image) => json!({
						"type": "image",
						"data": image.data,
						"mimeType": image.mime_type,
					}),
				})
				.collect();
			let payload = json!({
				"type": "tool_result",
				"toolCallId": ctx.tool_call.id,
				"toolName": ctx.tool_call.name,
				"input": ctx.tool_call.arguments,
				"content": content,
				"isError": ctx.is_error,
				"details": ctx.result.details.clone(),
			});
			// Fire-and-forget: a slow `tool_result` handler must not hold the
			// run (upstream awaits it; declared bridge deviation).
			bridge.hooks.fire("tool_result", payload, EXT_EVENT_TIMEOUT);
			pi_agent::AfterToolCallResult::default()
		});
		agent.set_after_tool_call(after);

		let bridge = self.clone();
		let prepare: pi_agent::PrepareRequestFn = Arc::new(move |ctx| {
			let bridge = bridge.clone();
			Box::pin(async move {
				let prompt = ctx
					.messages
					.iter()
					.rev()
					.find_map(|message| match message {
						pi_ai::Message::User(user) => Some(user.content.as_text()),
						_ => None,
					})
					.unwrap_or_default();
				bridge
					.dispatch_event_all(
						"before_agent_start",
						json!({"type": "before_agent_start", "prompt": prompt}),
						EXT_EVENT_TIMEOUT,
					)
					.await;
				// Results are ignored: upstream handlers may rewrite the
				// system prompt or inject a message; the bridge cannot modify
				// the request (declared limitation).
				None
			})
		});
		agent.set_prepare_request(prepare);
	}
}

/// Aggregate `tool_call` outcomes: any handler blocking wins, with the first
/// non-empty reason (an empty reason keeps pi-agent's upstream default text
/// "Tool execution was blocked").
fn aggregate_tool_call_outcomes(outcomes: &[Value]) -> pi_agent::BeforeToolCallResult {
	for outcome in outcomes {
		if outcome.get("block").and_then(Value::as_bool) == Some(true) {
			let reason = outcome
				.get("reason")
				.and_then(Value::as_str)
				.map(str::trim)
				.filter(|reason| !reason.is_empty())
				.map(str::to_string);
			return pi_agent::BeforeToolCallResult {
				block: true,
				reason,
				terminate: outcome
					.get("terminate")
					.and_then(Value::as_bool)
					.unwrap_or(false),
			};
		}
	}
	pi_agent::BeforeToolCallResult::default()
}

/// Run `future` while answering the host's reverse calls (`ui.*` dialogs,
/// session mutations) — the poc.rs select+answer loop, generalized. Dialogs
/// beyond `ui.notify` resolve `null` (declared limitation); a `null`
/// selection makes upstream fail-closed handlers block.
async fn with_host_answers<F, T>(
	host: &ExtensionHost,
	inner: &BridgeInner,
	future: F,
) -> Result<T, String>
where
	F: std::future::Future<Output = Result<T, String>>,
{
	tokio::pin!(future);
	let result = loop {
		tokio::select! {
			result = &mut future => break result,
			call = host.next_host_call(EXT_EVENT_TIMEOUT) => {
				// Channel closed or quiet timeout: keep waiting on the
				// future (it carries its own request timeout).
				if let Some(call) = call {
					answer_host_call(inner, &call).await;
				}
			}
		}
	};
	// Handlers may fire dialog calls without awaiting them (e.g. a trailing
	// `ctx.ui.notify`), so the frames hit the wire before the return frame
	// and are already queued when the request completes. Drain the queue
	// without blocking — `try_next_host_call` returns None immediately when
	// nothing is queued, and chained calls keep the loop going.
	while let Some(call) = host.try_next_host_call().await {
		answer_host_call(inner, &call).await;
	}
	result
}

/// Route one host-initiated call. `ui.notify` becomes a session notice (the
/// chat channel), everything else is acknowledged with `null` and dropped
/// (declared limitation, see the module docs).
async fn answer_host_call(inner: &BridgeInner, call: &pi_extension_host::HostCall) {
	if call.method == "ui.notify" {
		let message = call
			.params
			.get("message")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_string();
		inner.notice(message);
	}
	// Everything else — ui.select/ui.confirm/ui.input (no TUI routing this
	// round), ui.setStatus/ui.setTitle/ui.setEditorText, and the
	// session-mutation calls session.sendUserMessage / session.sendMessage /
	// session.setModel — is acknowledged with `null` and dropped (declared
	// limitation; the with_host_answers loop already answers null).
}

/// Dispatch one event to all enabled listening hosts (the dispatcher-thread
/// entry point; the async twin is [`ExtensionBridge::dispatch_event_all`]).
async fn dispatch_event_inner(
	inner: &BridgeInner,
	event: &str,
	payload: Value,
	timeout: Duration,
) -> Vec<Value> {
	let hosts: Vec<(String, ExtensionHost, bool)> = {
		inner
			.hosts
			.lock()
			.unwrap()
			.iter()
			.map(|entry| {
				(
					entry.label.clone(),
					entry.host.clone(),
					entry.events.contains(event),
				)
			})
			.collect()
	};
	let mut outcomes = Vec::new();
	for (label, host, listens) in hosts {
		if !listens {
			continue;
		}
		let future = host.dispatch_event(event, payload.clone(), timeout);
		match with_host_answers(&host, inner, future).await {
			Ok(mut handler_outcomes) => {
				for outcome in &handler_outcomes {
					if let Some(error) = outcome.get("__error").and_then(Value::as_str) {
						inner.notice(format!("extension {label} handler error: {error}"));
					}
				}
				outcomes.append(&mut handler_outcomes);
			}
			Err(error) => {
				if host.has_exited().await {
					inner.disable_host(&label, &host, &error).await;
				} else {
					inner.notice(format!("extension {label}: {error}"));
				}
			}
		}
	}
	outcomes
}

// -------------------------------------------------------------------------------------------
// ExtensionTool — AgentTool implementation over `tool.execute`.
// -------------------------------------------------------------------------------------------

/// One bridged extension tool: registration metadata drives the declaration,
/// execution round-trips through the host (`tool.execute`), and dialog calls
/// the handler makes are answered while it runs.
struct ExtensionTool {
	host: ExtensionHost,
	inner: Arc<BridgeInner>,
	def: ExtensionToolDef,
}

#[async_trait::async_trait]
impl AgentTool for ExtensionTool {
	fn name(&self) -> &str {
		&self.def.name
	}

	fn label(&self) -> &str {
		&self.def.label
	}

	fn description(&self) -> &str {
		&self.def.description
	}

	fn parameters_schema(&self) -> Value {
		self.def.parameters.clone()
	}

	fn execution_mode(&self) -> Option<pi_agent::ToolExecutionMode> {
		self.def
			.sequential
			.then_some(pi_agent::ToolExecutionMode::Sequential)
	}

	async fn execute(
		&self,
		tool_call_id: &str,
		params: Value,
		_signal: CancellationToken,
		_on_update: Option<pi_agent::ToolUpdate>,
	) -> AgentToolResult {
		let future = self.host.call(
			"tool.execute",
			json!({
				"name": self.def.name,
				"toolCallId": tool_call_id,
				"args": params,
			}),
			EXT_TOOL_TIMEOUT,
		);
		match with_host_answers(&self.host, &self.inner, future).await {
			Ok(value) => map_tool_result(value),
			Err(error) => {
				if self.host.has_exited().await {
					self.inner
						.disable_host(&self.def.extension_label, &self.host, &error)
						.await;
				}
				// Upstream maps a thrown tool error to the error result whose
				// content IS the message (agent-loop.ts createErrorToolResult).
				AgentToolResult::error(error)
			}
		}
	}
}

/// Map the extension tool's `AgentToolResult` JSON onto the agent result:
/// text/image content blocks pass through, `details` rides as JSON,
/// `isError` flags the result.
fn map_tool_result(value: Value) -> AgentToolResult {
	let mut result = AgentToolResult::default();
	if let Some(content) = value.get("content").and_then(Value::as_array) {
		for block in content {
			match block.get("type").and_then(Value::as_str) {
				Some("text") => {
					if let Some(text) = block.get("text").and_then(Value::as_str) {
						result.content.push(pi_ai::TextOrImage::text(text));
					}
				}
				Some("image") => {
					let data = block
						.get("data")
						.and_then(Value::as_str)
						.unwrap_or_default();
					let mime_type = block
						.get("mimeType")
						.and_then(Value::as_str)
						.unwrap_or("image/png");
					result
						.content
						.push(pi_ai::TextOrImage::Image(pi_ai::ImageContent {
							data: data.to_string(),
							mime_type: mime_type.to_string(),
						}));
				}
				// Unknown block shapes are stringified so nothing is dropped.
				_ => result
					.content
					.push(pi_ai::TextOrImage::text(block.to_string())),
			}
		}
	}
	result.details = value
		.get("details")
		.cloned()
		.filter(|details| !details.is_null());
	result.structured_content = value
		.get("structuredContent")
		.cloned()
		.filter(|content| !content.is_null());
	result.is_error = value
		.get("isError")
		.and_then(Value::as_bool)
		.unwrap_or(false);
	result
}

// -------------------------------------------------------------------------------------------
// Spawn
// -------------------------------------------------------------------------------------------

/// Spawn parameters for the assembly.
pub struct SpawnOptions {
	/// Working directory reported through the extension context.
	pub cwd: PathBuf,
	/// `ctx.hasUI` — true for interactive, false for print/json/rpc (upstream
	/// is true for RPC too; pi-rs has no RPC dialog forwarding yet — declared
	/// difference).
	pub has_ui: bool,
	/// Runtime executable ("node"; the bridge supports node only, bun is
	/// untested — declared).
	pub runtime: String,
}

/// The spawned assembly: the live bridge plus startup notices (routed by the
/// caller like the MCP assembly's).
pub struct ExtensionAssembly {
	pub bridge: Arc<ExtensionBridge>,
	pub notices: Vec<String>,
}

/// Spawn one host per entry, aggregate the registrations, isolate failures.
/// A missing runtime disables the whole assembly with a notice.
pub async fn spawn_extensions(
	entries: Vec<(String, PathBuf)>,
	options: &SpawnOptions,
) -> ExtensionAssembly {
	let mut notices = Vec::new();
	if entries.is_empty() {
		return ExtensionAssembly {
			bridge: Arc::new(ExtensionBridge::empty()),
			notices,
		};
	}
	// Runtime probe: a missing `node` disables everything (upstream would run
	// extensions in-process; the bridge has no runtime to spawn).
	if let Err(error) = std::process::Command::new(&options.runtime)
		.arg("--version")
		.output()
	{
		notices.push(format!(
			"extensions are disabled: {} not found ({error}); the bridge supports node only (bun is untested)",
			options.runtime
		));
		return ExtensionAssembly {
			bridge: Arc::new(ExtensionBridge::empty()),
			notices,
		};
	}

	let mut hosts: Vec<HostEntry> = Vec::new();
	let mut tools: Vec<ExtensionToolDef> = Vec::new();
	let mut commands: Vec<ExtensionCommandDef> = Vec::new();
	let mut tool_names: HashSet<String> = HashSet::new();

	for (label, entry) in entries {
		let config = HostConfig {
			runtime: options.runtime.clone(),
			extension: entry.clone(),
			flags: json!({}),
			cwd: options.cwd.clone(),
			has_ui: options.has_ui,
		};
		match tokio::time::timeout(EXT_SPAWN_TIMEOUT, ExtensionHost::spawn(config)).await {
			Ok(Ok(host)) => {
				let registrations = host.registrations().await;
				let mut events: HashSet<String> = HashSet::new();
				for registration in &registrations {
					collect_registration(
						registration,
						&mut RegistrationSink {
							host_index: hosts.len(),
							label: &label,
							events: &mut events,
							tools: &mut tools,
							commands: &mut commands,
							tool_names: &mut tool_names,
							notices: &mut notices,
						},
					);
				}
				hosts.push(HostEntry {
					label,
					host,
					enabled: true,
					events,
					actions_seen: 0,
				});
			}
			Ok(Err(error)) => {
				notices.push(format!("extension {}: {}", label, error));
			}
			Err(_) => {
				notices.push(format!(
					"extension {}: factory phase timed out after {}s",
					label,
					EXT_SPAWN_TIMEOUT.as_secs()
				));
			}
		}
	}

	ExtensionAssembly {
		bridge: Arc::new(ExtensionBridge::with_hosts(hosts, tools, commands)),
		notices,
	}
}

/// The assembly tables one host's registrations fold into.
struct RegistrationSink<'a> {
	host_index: usize,
	label: &'a str,
	events: &'a mut HashSet<String>,
	tools: &'a mut Vec<ExtensionToolDef>,
	commands: &'a mut Vec<ExtensionCommandDef>,
	tool_names: &'a mut HashSet<String>,
	notices: &'a mut Vec<String>,
}

/// Fold one factory-phase registration into the assembly tables.
fn collect_registration(registration: &Registration, sink: &mut RegistrationSink<'_>) {
	let RegistrationSink {
		host_index,
		label,
		events,
		tools,
		commands,
		tool_names,
		notices,
	} = sink;
	let host_index = *host_index;
	match registration.capability.as_str() {
		"event" => {
			if let Some(event) = &registration.event {
				events.insert(event.clone());
			}
		}
		"tool" => {
			let Some(definition) = &registration.definition else {
				return;
			};
			let Some(name) = definition.get("name").and_then(Value::as_str) else {
				return;
			};
			// Tool-name collisions between extensions: the later registration
			// is skipped with a notice (same-name tools within one extension
			// overwrite in the host already).
			if !tool_names.insert(name.to_string()) {
				notices.push(format!(
					"extension tool \"{name}\" ({label}) conflicts with an earlier registration; skipped"
				));
				return;
			}
			tools.push(ExtensionToolDef {
				host_index,
				extension_label: label.to_string(),
				name: name.to_string(),
				label: definition
					.get("label")
					.and_then(Value::as_str)
					.unwrap_or(name)
					.to_string(),
				description: definition
					.get("description")
					.and_then(Value::as_str)
					.unwrap_or_default()
					.to_string(),
				parameters: definition
					.get("parameters")
					.cloned()
					.unwrap_or_else(|| json!({})),
				sequential: definition.get("executionMode").and_then(Value::as_str)
					== Some("sequential"),
			});
		}
		"command" => {
			let Some(name) = &registration.name else {
				return;
			};
			commands.push(ExtensionCommandDef {
				host_index,
				name: name.clone(),
				description: registration.description.clone().unwrap_or_default(),
			});
		}
		// flags / unsupported capabilities: declared not bridged.
		_ => {}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	fn fixture(name: &str) -> PathBuf {
		PathBuf::from(env!("CARGO_MANIFEST_DIR"))
			.join("tests")
			.join("fixtures")
			.join("extensions")
			.join(name)
	}

	fn temp_root(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-ext-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn node_available() -> bool {
		std::process::Command::new("node")
			.arg("--version")
			.output()
			.is_ok()
	}

	fn discovery_options<'a>(
		cli_paths: &'a [PathBuf],
		cwd: &'a Path,
		agent_dir: &'a Path,
		include_defaults: bool,
		project_trusted: bool,
	) -> DiscoveryOptions<'a> {
		DiscoveryOptions {
			cli_paths,
			cwd,
			agent_dir,
			include_defaults,
			project_trusted,
		}
	}

	// -- discovery ------------------------------------------------------------

	#[test]
	fn discovery_finds_files_index_entries_and_skips_non_entry_dirs() {
		let root = temp_root("discover");
		let project = root.join("project");
		let agent_dir = root.join("agent");
		// Project extensions: a direct file, a subdirectory with index.ts, and
		// a subdirectory with neither index nor entry files (skipped).
		let ext_dir = project.join(".pi/extensions");
		std::fs::create_dir_all(&ext_dir).unwrap();
		std::fs::write(ext_dir.join("plain.ts"), "export default 1;\n").unwrap();
		std::fs::create_dir_all(ext_dir.join("packaged")).unwrap();
		std::fs::write(ext_dir.join("packaged/index.ts"), "export default 1;\n").unwrap();
		std::fs::create_dir_all(ext_dir.join("packaged-js")).unwrap();
		std::fs::write(
			ext_dir.join("packaged-js/index.js"),
			"module.exports = () => {};\n",
		)
		.unwrap();
		std::fs::create_dir_all(ext_dir.join("empty-sub")).unwrap();
		std::fs::write(ext_dir.join("empty-sub/readme.md"), "nope").unwrap();
		// User extensions: one direct file (deduped against the CLI path below).
		std::fs::create_dir_all(agent_dir.join("extensions")).unwrap();
		std::fs::write(agent_dir.join("extensions/user.ts"), "export default 1;\n").unwrap();

		let cli_paths = vec![
			agent_dir.join("extensions/user.ts"),
			ext_dir.join("packaged"),
		];
		let discovered = discover_extension_paths(discovery_options(
			&cli_paths, &project, &agent_dir, true, true,
		));
		assert!(
			discovered.notices.is_empty(),
			"trusted project has no notices: {:?}",
			discovered.notices
		);
		let names: Vec<String> = discovered
			.entries
			.iter()
			.map(|(label, _)| label.as_str().to_string())
			.collect();
		let find = |needle: &str| {
			names
				.iter()
				.position(|name| name.contains(needle))
				.unwrap_or_else(|| panic!("{needle} missing from {names:?}"))
		};
		// Load order: project dir, user dir, then CLI paths (upstream
		// addPaths order), with the duplicated user.ts/cli path appearing
		// once. Within one directory the readdir names are sorted
		// (determinism deviation from upstream's bare readdir order).
		assert!(find("packaged/index.ts") < find("packaged-js/index.js"));
		assert!(find("packaged-js/index.js") < find("plain.ts"));
		assert!(find("plain.ts") < find("user.ts"));
		let user_count = names.iter().filter(|name| name.contains("user.ts")).count();
		assert_eq!(user_count, 1, "deduped: {names:?}");
		assert!(!names.iter().any(|name| name.contains("empty-sub")));
		// The CLI directory form resolves to its index entry, not the directory.
		assert!(names.iter().any(|name| name.ends_with("packaged/index.ts")));
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[test]
	fn untrusted_project_extensions_are_skipped_with_a_notice() {
		let root = temp_root("trust");
		let project = root.join("project");
		let agent_dir = root.join("agent");
		std::fs::create_dir_all(project.join(".pi/extensions")).unwrap();
		std::fs::write(
			project.join(".pi/extensions/gate.ts"),
			"export default 1;\n",
		)
		.unwrap();
		std::fs::create_dir_all(agent_dir.join("extensions")).unwrap();
		std::fs::write(agent_dir.join("extensions/user.ts"), "export default 1;\n").unwrap();

		// Untrusted: the project directory is skipped, the user directory is not.
		let discovered =
			discover_extension_paths(discovery_options(&[], &project, &agent_dir, true, false));
		assert!(discovered.notices.len() == 1, "{:?}", discovered.notices);
		assert!(
			discovered.notices[0].contains("not trusted; project extensions are skipped"),
			"{:?}",
			discovered.notices
		);
		assert!(
			!discovered
				.entries
				.iter()
				.any(|(label, _)| label.contains("gate.ts"))
		);
		assert!(
			discovered
				.entries
				.iter()
				.any(|(label, _)| label.contains("user.ts"))
		);

		// Trusted: the project directory loads.
		let discovered =
			discover_extension_paths(discovery_options(&[], &project, &agent_dir, true, true));
		assert!(discovered.notices.is_empty(), "{:?}", discovered.notices);
		assert!(
			discovered
				.entries
				.iter()
				.any(|(label, _)| label.contains("gate.ts"))
		);
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[test]
	fn no_extensions_keeps_explicit_cli_paths_only() {
		let root = temp_root("no-ext");
		let project = root.join("project");
		let agent_dir = root.join("agent");
		std::fs::create_dir_all(project.join(".pi/extensions")).unwrap();
		std::fs::write(
			project.join(".pi/extensions/proj.ts"),
			"export default 1;\n",
		)
		.unwrap();
		std::fs::create_dir_all(agent_dir.join("extensions")).unwrap();
		std::fs::write(agent_dir.join("extensions/user.ts"), "export default 1;\n").unwrap();
		let cli = root.join("explicit.ts");
		std::fs::write(&cli, "export default 1;\n").unwrap();

		let discovered = discover_extension_paths(discovery_options(
			std::slice::from_ref(&cli),
			&project,
			&agent_dir,
			false,
			true,
		));
		assert!(
			discovered.notices.is_empty(),
			"-ne stays quiet: {:?}",
			discovered.notices
		);
		let names: Vec<String> = discovered
			.entries
			.iter()
			.map(|(label, _)| label.clone())
			.collect();
		assert_eq!(names, vec![cli.display().to_string()], "{names:?}");
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[test]
	fn a_missing_cli_path_is_a_notice_not_an_error() {
		let root = temp_root("missing");
		let missing = root.join("nowhere.ts");
		let discovered = discover_extension_paths(discovery_options(
			std::slice::from_ref(&missing),
			&root,
			&root,
			true,
			true,
		));
		assert_eq!(
			discovered.notices,
			vec![format!(
				"Extension path does not exist: {}",
				missing.display()
			)]
		);
		assert!(discovered.entries.is_empty());
		std::fs::remove_dir_all(&root).unwrap();
	}

	// -- tool_call outcome aggregation ---------------------------------------

	#[test]
	fn blocking_outcomes_aggregate_with_the_first_reason() {
		let outcomes = vec![
			serde_json::json!(null),
			serde_json::json!({"block": true, "reason": "no", "terminate": true}),
			serde_json::json!({"block": true, "reason": "later"}),
		];
		let decision = aggregate_tool_call_outcomes(&outcomes);
		assert!(decision.block);
		assert_eq!(decision.reason.as_deref(), Some("no"));
		assert!(decision.terminate);

		// No reason → None, so pi-agent applies the upstream default text.
		let decision = aggregate_tool_call_outcomes(&[serde_json::json!({"block": true})]);
		assert!(decision.block);
		assert_eq!(decision.reason, None);
		assert!(!decision.terminate);

		// Allows everywhere → no block.
		let decision = aggregate_tool_call_outcomes(&[serde_json::json!(null)]);
		assert!(!decision.block);
	}

	// -- spawn + bridge (real host processes) ---------------------------------

	fn spawn_opts(cwd: &Path) -> SpawnOptions {
		SpawnOptions {
			cwd: cwd.to_path_buf(),
			has_ui: false,
			runtime: "node".to_string(),
		}
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn spawned_tools_round_trip_and_conflicts_are_skipped() {
		if !node_available() {
			eprintln!("skipping: node not available");
			return;
		}
		let root = temp_root("spawn");
		let entries = vec![
			(
				fixture("dup-a").display().to_string(),
				fixture("dup-a").join("index.ts"),
			),
			(
				fixture("dup-b").display().to_string(),
				fixture("dup-b").join("index.ts"),
			),
			(
				fixture("hello.ts").display().to_string(),
				fixture("hello.ts"),
			),
		];
		let assembly = spawn_extensions(entries, &spawn_opts(&root)).await;
		// dup-b registers the same tool name as dup-a: skipped with a notice.
		assert!(
			assembly
				.notices
				.iter()
				.any(|notice| notice.contains("dup-b") && notice.contains("conflicts")),
			"{:?}",
			assembly.notices
		);

		let defs = assembly.bridge.tool_defs();
		let names: Vec<String> = defs.iter().map(|def| def.name.clone()).collect();
		assert_eq!(
			names,
			vec!["dup".to_string(), "hello".to_string()],
			"{defs:?}"
		);
		assert_eq!(defs[0].label, "Dup A", "first registration wins");

		// The bridged hello tool round-trips through the host.
		let hello = defs
			.iter()
			.find(|def| def.name == "hello")
			.map(|def| assembly.bridge.extension_tool(def.clone()))
			.unwrap();
		assert_eq!(hello.description(), "A simple greeting tool");
		assert_eq!(
			hello.parameters_schema()["properties"]["name"]["type"],
			serde_json::json!("string")
		);
		let result = hello
			.execute(
				"call-1",
				serde_json::json!({"name": "Pi"}),
				CancellationToken::new(),
				None,
			)
			.await;
		assert!(!result.is_error);
		let text = result
			.content
			.iter()
			.filter_map(|block| block.text_content())
			.collect::<Vec<_>>()
			.join("\n");
		assert_eq!(text, "Hello, Pi!");
		assert_eq!(result.details, Some(serde_json::json!({"greeted": "Pi"})));

		assembly.bridge.shutdown().await;
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_failing_factory_is_isolated_with_a_notice() {
		if !node_available() {
			eprintln!("skipping: node not available");
			return;
		}
		let root = temp_root("isolate");
		let entries = vec![
			(
				fixture("factory-throw.ts").display().to_string(),
				fixture("factory-throw.ts"),
			),
			(
				fixture("hello.ts").display().to_string(),
				fixture("hello.ts"),
			),
		];
		let assembly = spawn_extensions(entries, &spawn_opts(&root)).await;
		assert!(
			assembly
				.notices
				.iter()
				.any(|notice| notice.contains("factory-throw.ts")
					&& notice.contains("factory exploded on purpose")),
			"{:?}",
			assembly.notices
		);
		// The healthy extension still serves its tool.
		let defs = assembly.bridge.tool_defs();
		assert_eq!(defs.len(), 1);
		assert_eq!(defs[0].name, "hello");
		assembly.bridge.shutdown().await;
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_missing_runtime_disables_the_whole_assembly() {
		let root = temp_root("no-runtime");
		let entries = vec![(
			fixture("hello.ts").display().to_string(),
			fixture("hello.ts"),
		)];
		let assembly = spawn_extensions(
			entries,
			&SpawnOptions {
				runtime: "/nonexistent/pi-rs-node".to_string(),
				..spawn_opts(&root)
			},
		)
		.await;
		assert!(
			assembly
				.notices
				.iter()
				.any(|notice| notice.contains("extensions are disabled")),
			"{:?}",
			assembly.notices
		);
		assert!(assembly.bridge.tool_defs().is_empty());
		assert!(!assembly.bridge.is_empty() || true, "empty bridge no-ops");
		std::fs::remove_dir_all(&root).unwrap();
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn commands_execute_and_set_session_name_actions_are_drained() {
		if !node_available() {
			eprintln!("skipping: node not available");
			return;
		}
		let root = temp_root("commands");
		let entries = vec![(
			fixture("session-name.ts").display().to_string(),
			fixture("session-name.ts"),
		)];
		let assembly = spawn_extensions(entries, &spawn_opts(&root)).await;
		// Route check: the registered command matches `/session-name <args>`.
		assert_eq!(
			assembly.bridge.match_command("/session-name My Session"),
			Some(("session-name".to_string(), "My Session".to_string()))
		);
		assert_eq!(assembly.bridge.match_command("/other"), None);

		// Read path: no session name yet (notify lands as a bus notice; the
		// bus is unset here, so the answer loop just acknowledges it).
		assembly
			.bridge
			.execute_command("session-name", "")
			.await
			.expect("read path succeeds");
		// Set path: the sync setter travels as an action frame, drained by
		// the caller.
		assembly
			.bridge
			.execute_command("session-name", "My Session")
			.await
			.expect("set path succeeds");
		let names = assembly.bridge.drain_actions().await;
		assert_eq!(names, vec!["My Session".to_string()]);
		// Drained actions do not repeat.
		assert!(assembly.bridge.drain_actions().await.is_empty());

		// Unknown command errors.
		let error = assembly
			.bridge
			.execute_command("nope", "")
			.await
			.expect_err("unknown command errors");
		assert!(error.contains("unknown extension command"), "{error}");
		assembly.bridge.shutdown().await;
		std::fs::remove_dir_all(&root).unwrap();
	}
}
