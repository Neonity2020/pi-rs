//! Stateful agent — port of `pi/packages/agent/src/agent.ts`.
//!
//! `Agent` owns the transcript, emits lifecycle events over a broadcast
//! channel (the Rust analogue of `subscribe()`), executes tools, and exposes
//! steering/follow-up queues. Unlike the TS class it uses interior
//! mutability, so `Agent` is `Clone` and can be shared between the session
//! task and UI status probes while a run is in flight.

use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use pi_ai::{Message, Model, StreamFn, ThinkingLevel};

use crate::agent_loop::{run_agent_loop, AgentLoopRun, LoopOptions};
use crate::types::{
	default_model, AgentEvent, AgentTool, PendingMessageQueue, PromptInput, QueueMode,
	ToolExecutionMode, DEFAULT_TOOL_EXECUTION, EMPTY_USAGE,
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
	streaming: Arc<RwLock<Option<Message>>>,
	error_message: RwLock<Option<String>>,
	steering: Arc<Mutex<PendingMessageQueue>>,
	follow_up: Arc<Mutex<PendingMessageQueue>>,
	stream_fn: RwLock<StreamFn>,
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

impl Agent {
	pub fn new(options: AgentOptions) -> Self {
		let (event_tx, _) = broadcast::channel(1024);

		let mut messages = options.initial_messages.clone();
		let needs_leading_system = !matches!(messages.first(), Some(Message::System(_)));
		if needs_leading_system {
			let mut leading = pi_ai::SystemMessage::prompt(options.system_prompt.clone().unwrap_or_default());
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
			streaming: Arc::new(RwLock::new(None)),
			error_message: RwLock::new(None),
			steering: Arc::new(Mutex::new(PendingMessageQueue::new(options.steering_mode))),
			follow_up: Arc::new(Mutex::new(PendingMessageQueue::new(options.follow_up_mode))),
			stream_fn: RwLock::new(stream_fn),
			session_id: RwLock::new(options.session_id),
			event_tx,
			active_run: Mutex::new(None),
		};

		Self { inner: Arc::new(inner) }
	}

	/// Subscribe to agent lifecycle events (`Agent::subscribe` in the TS pi).
	pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
		self.inner.event_tx.subscribe()
	}

	/// Current state snapshot.
	pub fn snapshot(&self) -> AgentSnapshot {
		let messages = self.inner.messages.read().unwrap().clone();
		AgentSnapshot {
			model: self.inner.model.read().unwrap().clone(),
			thinking_level: *self.inner.thinking_level.read().unwrap(),
			tool_names: self.inner.tools.read().unwrap().iter().map(|t| t.name().to_string()).collect(),
			system_prompt: replay_system_prompt(&messages),
			streaming_message: self.inner.streaming.read().unwrap().clone(),
			queued_steering: self.inner.steering.lock().unwrap().len(),
			queued_follow_up: self.inner.follow_up.lock().unwrap().len(),
			error_message: self.inner.error_message.read().unwrap().clone(),
			messages,
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
		let messages = match input {
			PromptInput::Text { text } => vec![Message::user(text)],
			PromptInput::Messages(messages) => messages,
		};
		let opts = LoopOptions { signal: token, skip_initial_steering_poll: false };
		let result = self.run_loop(messages, opts).await;
		self.end_run();
		result
	}

	/// Continue from the current transcript (`Agent::continue()`): drains
	/// steering, then follow-ups; otherwise requires the last message to be a
	/// user or tool-result message.
	pub async fn continue_run(&self) -> anyhow::Result<()> {
		let token = self.begin_run()?;

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
				let opts = LoopOptions { signal: token, skip_initial_steering_poll: true };
				let result = self.run_loop(steering, opts).await;
				self.end_run();
				return result;
			}
			let follow_ups = self.inner.follow_up.lock().unwrap().drain();
			if !follow_ups.is_empty() {
				let opts = LoopOptions { signal: token, skip_initial_steering_poll: false };
				let result = self.run_loop(follow_ups, opts).await;
				self.end_run();
				return result;
			}
			self.end_run();
			return Err(anyhow::anyhow!("cannot continue from message role: assistant"));
		}

		if last_role == "system" {
			self.end_run();
			return Err(anyhow::anyhow!("no messages to continue from"));
		}

		let opts = LoopOptions { signal: token, skip_initial_steering_poll: false };
		let result = self.run_loop(Vec::new(), opts).await;
		self.end_run();
		result
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

	pub fn has_queued_messages(&self) -> bool {
		self.inner.steering.lock().unwrap().has_items()
			|| self.inner.follow_up.lock().unwrap().has_items()
	}

	pub fn clear_all_queues(&self) {
		self.inner.steering.lock().unwrap().clear();
		self.inner.follow_up.lock().unwrap().clear();
	}

	pub fn set_steering_mode(&self, mode: QueueMode) {
		self.inner.steering.lock().unwrap().mode = mode;
	}

	pub fn set_follow_up_mode(&self, mode: QueueMode) {
		self.inner.follow_up.lock().unwrap().mode = mode;
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

	pub fn set_stream_fn(&self, stream_fn: StreamFn) {
		*self.inner.stream_fn.write().unwrap() = stream_fn;
	}

	/// Replace the executable tool set. Differences from the tools declared in
	/// the transcript are announced to the model with a system message before
	/// the next request (TODO(pi): announcement message).
	pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
		*self.inner.tools.write().unwrap() = tools;
	}

	pub fn tool_names(&self) -> Vec<String> {
		self.inner.tools.read().unwrap().iter().map(|t| t.name().to_string()).collect()
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
		}
	}

	async fn run_loop(&self, prompt_messages: Vec<Message>, opts: LoopOptions) -> anyhow::Result<()> {
		let run = self.build_run();
		let token = opts.signal.clone();
		let result = run_agent_loop(&run, prompt_messages, opts).await;
		if let Err(error) = &result {
			self.handle_run_failure(error, token.is_cancelled()).await;
		}
		result
	}

	/// Mirror of `handleRunFailure`: synthesize a full error turn so listeners
	/// observe the normal event sequence.
	async fn handle_run_failure(&self, error: &anyhow::Error, aborted: bool) {
		let model = self.inner.model.read().unwrap().clone();
		let stop_reason = if aborted { pi_ai::StopReason::Aborted } else { pi_ai::StopReason::Error };
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
		let _ = tx.send(AgentEvent::MessageStart { message: message.clone() });
		let _ = tx.send(AgentEvent::MessageEnd { message: message.clone() });
		let _ = tx.send(AgentEvent::TurnEnd { message: message.clone(), tool_results: Vec::new() });
		let _ = tx.send(AgentEvent::AgentEnd { messages: vec![message] });
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
