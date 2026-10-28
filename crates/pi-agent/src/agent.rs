//! Stateful agent — port of `pi/packages/agent/src/agent.ts`.
//!
//! `Agent` owns the transcript, emits lifecycle events over a broadcast
//! channel (the Rust analogue of `subscribe()`), executes tools, and exposes
//! steering/follow-up queues. Unlike the TS class it uses interior
//! mutability, so `Agent` is `Clone` and can be shared between the session
//! task and UI status probes while a run is in flight.

use std::sync::{
	Arc, Mutex, RwLock,
	atomic::{AtomicUsize, Ordering},
};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use pi_ai::{Message, Model, StreamFn, ThinkingLevel, Tool, ToolReference};

use crate::agent_loop::{
	AgentLoopRun, FinishTurnFn, LoopOptions, PrepareNextTurnFn, PrepareRequestFn, run_agent_loop,
};
use crate::types::{
	AfterToolCallFn, AgentEvent, AgentTool, BeforeToolCallFn, DEFAULT_TOOL_EXECUTION, EMPTY_USAGE,
	PendingMessageQueue, PromptInput, QueueMode, ToolExecutionMode, default_model,
};

/// Options for constructing an [`Agent`] — mirrors `AgentOptions`.
#[derive(Clone)]
pub struct AgentOptions {
	pub model: Model,
	pub stream_fn: Option<StreamFn>,
	pub thinking_level: ThinkingLevel,
	pub tools: Vec<Arc<dyn AgentTool>>,
	/// Seeds the leading system message unless `messages` already starts with one.
	pub system_prompt: Option<String>,
	pub initial_messages: Vec<Message>,
	pub tool_execution: ToolExecutionMode,
	pub steering_mode: QueueMode,
	pub follow_up_mode: QueueMode,
	pub session_id: Option<String>,
}

impl Default for AgentOptions {
	fn default() -> Self {
		Self {
			model: default_model(),
			stream_fn: None,
			thinking_level: ThinkingLevel::Off,
			tools: Vec::new(),
			system_prompt: None,
			initial_messages: Vec::new(),
			tool_execution: DEFAULT_TOOL_EXECUTION,
			steering_mode: QueueMode::default(),
			follow_up_mode: QueueMode::default(),
			session_id: None,
		}
	}
}

/// Read-only view of the agent's current state (`AgentState` in the TS pi).
#[derive(Clone)]
pub struct AgentSnapshot {
	pub model: Model,
	pub thinking_level: ThinkingLevel,
	pub tool_names: Vec<String>,
	pub messages: Vec<Message>,
	pub usage_valid_from: usize,
	pub streaming_message: Option<Message>,
	pub system_prompt: String,
	pub queued_steering: usize,
	pub queued_follow_up: usize,
	pub error_message: Option<String>,
}

struct AgentInner {
	model: RwLock<Model>,
	thinking_level: RwLock<ThinkingLevel>,
	tools: RwLock<Vec<Arc<dyn AgentTool>>>,
	messages: Arc<RwLock<Vec<Message>>>,
	usage_valid_from: AtomicUsize,
	streaming: Arc<RwLock<Option<Message>>>,
	error_message: RwLock<Option<String>>,
	steering: Arc<Mutex<PendingMessageQueue>>,
	follow_up: Arc<Mutex<PendingMessageQueue>>,
	stream_fn: RwLock<StreamFn>,
	prepare_next_turn: RwLock<Option<PrepareNextTurnFn>>,
	prepare_request: RwLock<Option<PrepareRequestFn>>,
	finish_turn: RwLock<Option<FinishTurnFn>>,
	before_tool_call: RwLock<Option<BeforeToolCallFn>>,
	after_tool_call: RwLock<Option<AfterToolCallFn>>,
	tool_execution: ToolExecutionMode,
	session_id: RwLock<Option<String>>,
	event_tx: broadcast::Sender<AgentEvent>,
	/// Cancellation token of the active run; `None` when idle. Std mutex:
	/// never held across an await.
	active_run: Mutex<Option<CancellationToken>>,
}

/// The agent. Clone to share; all mutation goes through interior mutability.
#[derive(Clone)]
pub struct Agent {
	inner: Arc<AgentInner>,
}

struct RunGuard(Agent);

impl Drop for RunGuard {
	fn drop(&mut self) {
		self.0.end_run();
	}
}

impl Agent {
	pub fn new(options: AgentOptions) -> Self {
		let (event_tx, _) = broadcast::channel(1024);

		let mut messages = options.initial_messages.clone();
		let needs_leading_system = !matches!(messages.first(), Some(Message::System(_)));
		if needs_leading_system {
			let mut leading =
				pi_ai::SystemMessage::prompt(options.system_prompt.clone().unwrap_or_default());
			if !options.tools.is_empty() {
				leading.tools_added =
					Some(options.tools.iter().map(|t| t.to_declaration()).collect());
			}
			messages.insert(0, Message::System(leading));
		}

		let stream_fn: StreamFn = options
			.stream_fn
			.unwrap_or_else(|| pi_ai::providers::stream_fn_for(&options.model));

		let inner = AgentInner {
			model: RwLock::new(options.model.clone()),
			thinking_level: RwLock::new(options.thinking_level),
			tools: RwLock::new(options.tools),
			messages: Arc::new(RwLock::new(messages)),
			usage_valid_from: AtomicUsize::new(0),
			streaming: Arc::new(RwLock::new(None)),
			error_message: RwLock::new(None),
			steering: Arc::new(Mutex::new(PendingMessageQueue::new(options.steering_mode))),
			follow_up: Arc::new(Mutex::new(PendingMessageQueue::new(options.follow_up_mode))),
			stream_fn: RwLock::new(stream_fn),
			prepare_next_turn: RwLock::new(None),
			prepare_request: RwLock::new(None),
			finish_turn: RwLock::new(None),
			before_tool_call: RwLock::new(None),
			after_tool_call: RwLock::new(None),
			tool_execution: options.tool_execution,
			session_id: RwLock::new(options.session_id),
			event_tx,
			active_run: Mutex::new(None),
		};

		Self {
			inner: Arc::new(inner),
		}
	}

	/// Subscribe to agent lifecycle events (`Agent::subscribe` in the TS pi).
	pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
		self.inner.event_tx.subscribe()
	}

	/// Current state snapshot.
	pub fn snapshot(&self) -> AgentSnapshot {
		let transcript = self.inner.messages.read().unwrap();
		let usage_valid_from = self.inner.usage_valid_from.load(Ordering::SeqCst);
		let messages = transcript.clone();
		drop(transcript);
		AgentSnapshot {
			model: self.inner.model.read().unwrap().clone(),
			thinking_level: *self.inner.thinking_level.read().unwrap(),
			tool_names: self
				.inner
				.tools
				.read()
				.unwrap()
				.iter()
				.map(|t| t.name().to_string())
				.collect(),
			system_prompt: replay_system_prompt(&messages),
			streaming_message: self.inner.streaming.read().unwrap().clone(),
			queued_steering: self.inner.steering.lock().unwrap().len(),
			queued_follow_up: self.inner.follow_up.lock().unwrap().len(),
			error_message: self.inner.error_message.read().unwrap().clone(),
			messages,
			usage_valid_from,
		}
	}

	// -- run management ------------------------------------------------------

	/// True while the agent is processing a prompt or continuation.
	pub fn is_streaming(&self) -> bool {
		self.inner.active_run.lock().unwrap().is_some()
	}

	/// Active abort signal for the current run, if any.
	pub fn signal(&self) -> Option<CancellationToken> {
		self.inner.active_run.lock().unwrap().clone()
	}

	/// Abort the current run, if one is active.
	pub fn abort(&self) {
		if let Some(token) = self.inner.active_run.lock().unwrap().as_ref() {
			token.cancel();
		}
	}

	/// Start a new prompt from text or prebuilt messages. Rejects when a run
	/// is already active — use `steer`/`follow_up` to queue instead.
	pub async fn prompt(&self, input: PromptInput) -> anyhow::Result<()> {
		let token = self.begin_run()?;
		let _guard = RunGuard(self.clone());
		let messages = match input {
			PromptInput::Text { text } => vec![Message::user(text)],
			PromptInput::Messages(messages) => messages,
		};
		let opts = LoopOptions {
			signal: token,
			skip_initial_steering_poll: false,
		};
		self.run_loop(messages, opts).await
	}

	/// Continue from the current transcript (`Agent::continue()`): drains
	/// steering, then follow-ups; otherwise requires the last message to be a
	/// user or tool-result message.
	pub async fn continue_run(&self) -> anyhow::Result<()> {
		let token = self.begin_run()?;
		let _guard = RunGuard(self.clone());

		let last_role = self
			.inner
			.messages
			.read()
			.unwrap()
			.last()
			.map(|m| m.role())
			.unwrap_or("system");

		if last_role == "assistant" {
			let steering = self.inner.steering.lock().unwrap().drain();
			if !steering.is_empty() {
				let opts = LoopOptions {
					signal: token,
					skip_initial_steering_poll: true,
				};
				let result = self.run_loop(steering, opts).await;
				return result;
			}
			let follow_ups = self.inner.follow_up.lock().unwrap().drain();
			if !follow_ups.is_empty() {
				let opts = LoopOptions {
					signal: token,
					skip_initial_steering_poll: false,
				};
				let result = self.run_loop(follow_ups, opts).await;
				return result;
			}
			return Err(anyhow::anyhow!(
				"cannot continue from message role: assistant"
			));
		}

		if last_role == "system" {
			return Err(anyhow::anyhow!("no messages to continue from"));
		}

		let opts = LoopOptions {
			signal: token,
			skip_initial_steering_poll: false,
		};
		self.run_loop(Vec::new(), opts).await
	}

	// -- queues ---------------------------------------------------------------

	/// Queue a message to be injected after the current assistant turn.
	pub fn steer(&self, text: impl Into<String>) {
		self.steer_message(Message::user(text));
	}

	pub fn steer_message(&self, message: Message) {
		self.inner.steering.lock().unwrap().enqueue(message);
	}

	/// Queue a message to run only after the agent would otherwise stop.
	pub fn follow_up(&self, text: impl Into<String>) {
		self.follow_up_message(Message::user(text));
	}

	pub fn follow_up_message(&self, message: Message) {
		self.inner.follow_up.lock().unwrap().enqueue(message);
	}

	/// Preview the messages selected for the next turn without consuming them.
	pub fn peek_queued_messages(&self) -> Vec<Message> {
		let steering = self.inner.steering.lock().unwrap().peek();
		if !steering.is_empty() {
			return steering;
		}
		self.inner.follow_up.lock().unwrap().peek()
	}

	/// Preview the queued steering messages without consuming them.
	pub fn peek_steering_queue(&self) -> Vec<Message> {
		self.inner.steering.lock().unwrap().peek()
	}

	/// Preview the queued follow-up messages without consuming them.
	pub fn peek_follow_up_queue(&self) -> Vec<Message> {
		self.inner.follow_up.lock().unwrap().peek()
	}

	pub fn has_queued_messages(&self) -> bool {
		self.inner.steering.lock().unwrap().has_items()
			|| self.inner.follow_up.lock().unwrap().has_items()
	}

	/// Remove all queued steering messages.
	pub fn clear_steering_queue(&self) {
		self.inner.steering.lock().unwrap().clear();
	}

	/// Remove all queued follow-up messages.
	pub fn clear_follow_up_queue(&self) {
		self.inner.follow_up.lock().unwrap().clear();
	}

	pub fn clear_all_queues(&self) {
		self.clear_steering_queue();
		self.clear_follow_up_queue();
	}

	pub fn set_steering_mode(&self, mode: QueueMode) {
		self.inner.steering.lock().unwrap().mode = mode;
	}

	pub fn set_follow_up_mode(&self, mode: QueueMode) {
		self.inner.follow_up.lock().unwrap().mode = mode;
	}

	/// Current queue modes, for status probes (RPC `get_state`'s
	/// `steeringMode`/`followUpMode`).
	pub fn queue_modes(&self) -> (QueueMode, QueueMode) {
		(
			self.inner.steering.lock().unwrap().mode,
			self.inner.follow_up.lock().unwrap().mode,
		)
	}

	// -- configuration ---------------------------------------------------------

	pub fn set_model(&self, model: Model) {
		*self.inner.model.write().unwrap() = model;
	}

	pub fn model(&self) -> Model {
		self.inner.model.read().unwrap().clone()
	}

	pub fn set_thinking_level(&self, level: ThinkingLevel) {
		*self.inner.thinking_level.write().unwrap() = level;
	}

	pub fn thinking_level(&self) -> ThinkingLevel {
		*self.inner.thinking_level.read().unwrap()
	}

	pub fn stream_fn(&self) -> StreamFn {
		self.inner.stream_fn.read().unwrap().clone()
	}

	pub fn set_stream_fn(&self, stream_fn: StreamFn) {
		*self.inner.stream_fn.write().unwrap() = stream_fn;
	}

	/// Replace the whole transcript with `messages` — the mutation behind
	/// context compaction (`AgentSession.compact` swaps in
	/// `[system…, compaction summary]` after `compaction.ts` produced the
	/// summary). Refused while a run is active: the loop would keep appending
	/// past the replacement, which is why upstream only compacts an idle
	/// agent. The replacement is authoritative; queues are left untouched.
	pub fn replace_messages(&self, messages: Vec<Message>) -> anyhow::Result<()> {
		let mut transcript = self.inner.messages.write().unwrap();
		if self.inner.active_run.lock().unwrap().is_some() {
			anyhow::bail!("cannot replace messages while a run is active");
		}
		self.inner.usage_valid_from.store(0, Ordering::SeqCst);
		*transcript = messages;
		Ok(())
	}

	/// Atomically validate and persist a transcript replacement while excluding
	/// concurrent agent runs. The synchronous commit must not call Agent APIs.
	pub fn replace_messages_transactionally<F>(
		&self,
		expected: &[Message],
		replacement: Vec<Message>,
		commit: F,
	) -> anyhow::Result<()>
	where
		F: FnOnce() -> anyhow::Result<()>,
	{
		let mut transcript = self.inner.messages.write().unwrap();
		let active = self.inner.active_run.lock().unwrap();
		if active.is_some() {
			anyhow::bail!("cannot replace messages while a run is active");
		}
		if *transcript != expected {
			anyhow::bail!("transcript changed during compaction");
		}
		commit()?;
		self.inner
			.usage_valid_from
			.store(replacement.len(), Ordering::SeqCst);
		*transcript = replacement;
		Ok(())
	}

	/// Set the earliest transcript index with post-compaction provider usage.
	pub fn set_usage_valid_from(&self, index: usize) {
		self.inner.usage_valid_from.store(index, Ordering::SeqCst);
	}

	/// Replace the executable tool set. Differences from the tools declared in
	/// the transcript are announced to the model with a system message
	/// appended to the transcript (with lifecycle events) before the next
	/// request — the port of `declareToolChanges` for the stateful agent. A
	/// changed definition counts as a removal followed by an addition. While a
	/// run is active only the executable set is swapped (the active loop
	/// already holds a snapshot); the announcement is skipped, matching
	/// upstream where the difference is (re)computed at the next run start.
	pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
		let declarations: Vec<Tool> = tools.iter().map(|tool| tool.to_declaration()).collect();
		*self.inner.tools.write().unwrap() = tools;

		let declared = replay_declared_tools(&self.inner.messages.read().unwrap());
		let changes = tool_state_changes(&declared, &declarations);
		if changes.tools_added.is_empty() && changes.tools_removed.is_empty() {
			return;
		}
		if self.inner.active_run.lock().unwrap().is_some() {
			return;
		}

		// Upstream announcement format: a system message with empty content and
		// only the non-empty change lists set (`withToolChanges`).
		let mut announcement = pi_ai::SystemMessage::prompt("");
		if !changes.tools_added.is_empty() {
			announcement.tools_added = Some(changes.tools_added);
		}
		if !changes.tools_removed.is_empty() {
			announcement.tools_removed = Some(changes.tools_removed);
		}
		let message = Message::System(announcement);
		self.inner.messages.write().unwrap().push(message.clone());
		let _ = self.inner.event_tx.send(AgentEvent::MessageStart {
			message: message.clone(),
		});
		let _ = self.inner.event_tx.send(AgentEvent::MessageEnd { message });
	}

	/// Install the `prepare_next_turn` hook (mid-run compaction lives behind
	/// this in the coding-agent session). The loop swaps model/thinking and
	/// appends the returned messages before the next request.
	pub fn set_prepare_next_turn(&self, hook: PrepareNextTurnFn) {
		*self.inner.prepare_next_turn.write().unwrap() = Some(hook);
	}

	/// Install the `prepare_request` hook: runs immediately before every
	/// provider request, including the first, and may swap model/thinking.
	pub fn set_prepare_request(&self, hook: PrepareRequestFn) {
		*self.inner.prepare_request.write().unwrap() = Some(hook);
	}

	/// Install the `finish_turn` hook: runs after each completed turn (and the
	/// steering poll); `End` forces the run to end, `Continue` guarantees one
	/// more provider request.
	pub fn set_finish_turn(&self, hook: FinishTurnFn) {
		*self.inner.finish_turn.write().unwrap() = Some(hook);
	}

	/// Install the `before_tool_call` hook: runs before a tool executes;
	/// `block: true` replaces the execution with an error tool result.
	pub fn set_before_tool_call(&self, hook: BeforeToolCallFn) {
		*self.inner.before_tool_call.write().unwrap() = Some(hook);
	}

	/// Install the `after_tool_call` hook: runs after a tool executed, before
	/// `tool_execution_end`; provided fields replace the executed result's.
	pub fn set_after_tool_call(&self, hook: AfterToolCallFn) {
		*self.inner.after_tool_call.write().unwrap() = Some(hook);
	}

	pub fn tool_names(&self) -> Vec<String> {
		self.inner
			.tools
			.read()
			.unwrap()
			.iter()
			.map(|t| t.name().to_string())
			.collect()
	}

	pub fn session_id(&self) -> Option<String> {
		self.inner.session_id.read().unwrap().clone()
	}

	pub fn set_session_id(&self, id: Option<String>) {
		*self.inner.session_id.write().unwrap() = id;
	}

	// -- internals -------------------------------------------------------------

	fn begin_run(&self) -> anyhow::Result<CancellationToken> {
		let mut guard = self.inner.active_run.lock().unwrap();
		if guard.is_some() {
			anyhow::bail!(
				"agent is already processing a prompt; use steer()/follow_up() to queue messages, or wait for completion"
			);
		}
		let token = CancellationToken::new();
		*guard = Some(token.clone());
		drop(guard);
		*self.inner.error_message.write().unwrap() = None;
		Ok(token)
	}

	fn end_run(&self) {
		*self.inner.streaming.write().unwrap() = None;
		*self.inner.active_run.lock().unwrap() = None;
	}

	fn build_run(&self) -> AgentLoopRun {
		AgentLoopRun {
			model: self.inner.model.read().unwrap().clone(),
			thinking_level: *self.inner.thinking_level.read().unwrap(),
			tools: self.inner.tools.read().unwrap().clone(),
			stream_fn: self.inner.stream_fn.read().unwrap().clone(),
			session_id: self.inner.session_id.read().unwrap().clone(),
			messages: self.inner.messages.clone(),
			streaming: self.inner.streaming.clone(),
			steering: self.inner.steering.clone(),
			follow_up: self.inner.follow_up.clone(),
			event_tx: self.inner.event_tx.clone(),
			convert_to_llm: None,
			prepare_next_turn: self.inner.prepare_next_turn.read().unwrap().clone(),
			tool_execution: self.inner.tool_execution,
			prepare_request: self.inner.prepare_request.read().unwrap().clone(),
			finish_turn: self.inner.finish_turn.read().unwrap().clone(),
			before_tool_call: self.inner.before_tool_call.read().unwrap().clone(),
			after_tool_call: self.inner.after_tool_call.read().unwrap().clone(),
		}
	}

	async fn run_loop(
		&self,
		prompt_messages: Vec<Message>,
		opts: LoopOptions,
	) -> anyhow::Result<()> {
		let mut run = self.build_run();
		let token = opts.signal.clone();
		let result = run_agent_loop(&mut run, prompt_messages, opts).await;
		if let Err(error) = &result {
			self.handle_run_failure(error, token.is_cancelled()).await;
		}
		result
	}

	/// Mirror of `handleRunFailure`: synthesize a full error turn so listeners
	/// observe the normal event sequence.
	async fn handle_run_failure(&self, error: &anyhow::Error, aborted: bool) {
		let model = self.inner.model.read().unwrap().clone();
		let stop_reason = if aborted {
			pi_ai::StopReason::Aborted
		} else {
			pi_ai::StopReason::Error
		};
		let failure = pi_ai::AssistantMessage {
			content: vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
				text: String::new(),
				text_signature: None,
			})],
			api: model.api.as_str().to_string(),
			provider: model.provider.clone(),
			model: model.id.clone(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: EMPTY_USAGE,
			stop_reason,
			error_message: Some(error.to_string()),
			raw_stop_reason: None,
			end_turn: None,
			timestamp: pi_ai::types::now_ms(),
			duration_ms: None,
		};
		*self.inner.error_message.write().unwrap() = failure.error_message.clone();
		let message = Message::Assistant(failure);
		self.inner.messages.write().unwrap().push(message.clone());
		let tx = &self.inner.event_tx;
		let _ = tx.send(AgentEvent::MessageStart {
			message: message.clone(),
		});
		let _ = tx.send(AgentEvent::MessageEnd {
			message: message.clone(),
		});
		let _ = tx.send(AgentEvent::TurnEnd {
			message: message.clone(),
			tool_results: Vec::new(),
		});
		let _ = tx.send(AgentEvent::AgentEnd {
			messages: vec![message],
		});
	}
}

/// Replay every system message in order to compute the current prompt
/// (`getCurrentSystemPrompt`).
fn replay_system_prompt(messages: &[Message]) -> String {
	let mut prompt = String::new();
	for message in messages {
		if let Message::System(system) = message {
			match &system.content {
				pi_ai::SystemContent::Text(text) => prompt.push_str(text),
				pi_ai::SystemContent::Blocks(blocks) => {
					for block in blocks {
						prompt.push_str(&block.text);
					}
				}
			}
			if !prompt.is_empty() && !prompt.ends_with('\n') {
				prompt.push('\n');
			}
		}
	}
	prompt
}

/// Port of `getCurrentTools` (`ai/src/utils/transcript.ts`): resolve the tool
/// declarations of the transcript's system messages, preserving declaration
/// order. A removed name drops the tool; a re-added name replaces it in place.
fn replay_declared_tools(messages: &[Message]) -> Vec<Tool> {
	let mut tools: Vec<Tool> = Vec::new();
	for message in messages {
		let Message::System(system) = message else {
			continue;
		};
		for removed in system.tools_removed.iter().flatten() {
			tools.retain(|tool| tool.name != removed.name);
		}
		for added in system.tools_added.iter().flatten() {
			match tools.iter_mut().find(|tool| tool.name == added.name) {
				Some(existing) => *existing = added.clone(),
				None => tools.push(added.clone()),
			}
		}
	}
	tools
}

/// Port of `getToolStateChanges`: compare two complete tool states. A changed
/// definition is a removal followed by an addition.
fn tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
	ToolStateChanges {
		tools_added: current
			.iter()
			.filter(|tool| {
				previous
					.iter()
					.find(|candidate| candidate.name == tool.name)
					.is_none_or(|candidate| candidate != *tool)
			})
			.cloned()
			.collect(),
		tools_removed: previous
			.iter()
			.filter(|tool| {
				current
					.iter()
					.find(|candidate| candidate.name == tool.name)
					.is_none_or(|candidate| candidate != *tool)
			})
			.map(|tool| ToolReference {
				name: tool.name.clone(),
			})
			.collect(),
	}
}

/// Port of the upstream `ToolStateChanges`.
struct ToolStateChanges {
	tools_added: Vec<Tool>,
	tools_removed: Vec<ToolReference>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn transaction_excludes_concurrent_runs_and_rejects_failures() {
		let agent = Agent::new(AgentOptions::default());
		let original = agent.snapshot().messages;
		let called = std::sync::atomic::AtomicBool::new(false);
		assert!(
			agent
				.replace_messages_transactionally(&[], vec![], || {
					called.store(true, Ordering::SeqCst);
					Ok(())
				})
				.is_err()
		);
		assert!(!called.load(Ordering::SeqCst));
		assert!(
			agent
				.replace_messages_transactionally(&original, vec![], || anyhow::bail!("injected"))
				.is_err()
		);
		assert_eq!(agent.snapshot().messages, original);
		let competing = agent.clone();
		let (tx, rx) = std::sync::mpsc::channel();
		let mut thread = None;
		agent
			.replace_messages_transactionally(&original, vec![Message::user("new")], || {
				thread = Some(std::thread::spawn(move || {
					let acquired = competing.begin_run().is_ok();
					let _ = tx.send(acquired);
					if acquired {
						competing.end_run();
					}
				}));
				assert!(
					rx.recv_timeout(std::time::Duration::from_millis(30))
						.is_err()
				);
				Ok(())
			})
			.unwrap();
		assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap());
		thread.unwrap().join().unwrap();
		assert_eq!(agent.snapshot().messages[0].role(), "user");
	}

	#[test]
	fn replace_messages_swaps_transcript_and_snapshot_reflects_it() {
		let agent = Agent::new(AgentOptions::default());
		let replacement = vec![
			Message::System(pi_ai::SystemMessage::prompt("kept prompt")),
			Message::user("carry-over summary"),
		];
		agent.replace_messages(replacement.clone()).unwrap();
		assert_eq!(agent.snapshot().messages, replacement);
		// snapshot().system_prompt terminates each replayed system message with \n.
		assert_eq!(agent.snapshot().system_prompt, "kept prompt\n");
	}

	#[test]
	fn replacement_without_system_message_yields_empty_prompt() {
		let agent = Agent::new(AgentOptions {
			system_prompt: Some("old".into()),
			..Default::default()
		});
		agent
			.replace_messages(vec![Message::user("fresh")])
			.unwrap();
		assert_eq!(agent.snapshot().messages.len(), 1);
		assert_eq!(agent.snapshot().system_prompt, "");
	}

	/// The next run must build on the replacement, not the old transcript.
	#[tokio::test]
	async fn run_after_replacement_uses_the_new_history() {
		let agent = Agent::new(AgentOptions {
			stream_fn: Some(Arc::new(pi_ai::providers::mock::stream)),
			..Default::default()
		});
		agent
			.replace_messages(vec![
				Message::System(pi_ai::SystemMessage::prompt("compacted agent")),
				Message::user("carry-over summary"),
			])
			.unwrap();
		agent.prompt("again".into()).await.unwrap();
		let roles: Vec<&str> = agent
			.snapshot()
			.messages
			.iter()
			.map(Message::role)
			.collect();
		assert_eq!(roles, ["system", "user", "user", "assistant"]);
		let snapshot = agent.snapshot();
		let Message::Assistant(reply) = snapshot.messages.last().unwrap() else {
			panic!("expected assistant reply")
		};
		assert!(reply.text().contains("again"));
	}

	#[tokio::test]
	async fn replace_messages_is_refused_while_a_run_is_active() {
		let agent = Agent::new(AgentOptions {
			stream_fn: Some(Arc::new(|_, _, _| {
				Box::pin(futures::stream::pending::<pi_ai::AssistantMessageEvent>())
			})),
			..Default::default()
		});
		let worker = agent.clone();
		let task = tokio::spawn(async move { worker.prompt("hello".into()).await });
		tokio::time::timeout(std::time::Duration::from_secs(5), async {
			while !agent.is_streaming() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.unwrap();
		assert!(agent.replace_messages(Vec::new()).is_err());
		// The transcript is untouched: system + the prompt still in flight.
		assert_eq!(agent.snapshot().messages.len(), 2);
		task.abort();
		let _ = task.await;
		assert!(!agent.is_streaming());
		agent
			.replace_messages(vec![Message::user("fresh")])
			.unwrap();
		assert_eq!(agent.snapshot().messages, vec![Message::user("fresh")]);
	}
}
