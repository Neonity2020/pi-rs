//! pi-agent — stateful wrapper around the low-level agent loop.
//!
//! Rust port of `pi/packages/agent`: `Agent` owns the current transcript,
//! emits lifecycle events, executes tools, and exposes queueing APIs for
//! steering and follow-up messages. The loop itself lives in
//! [`agent_loop`] and mirrors `runAgentLoop` / `runAgentLoopContinue` from
//! `pi/packages/agent/src/agent-loop.ts`.

pub mod agent;
pub mod agent_loop;
pub mod types;

pub use agent::{Agent, AgentOptions, AgentSnapshot};
pub use agent_loop::{run_agent_loop, AgentLoopRun, LoopOptions};
pub use types::{
	AgentEvent, AgentTool, AgentToolResult, AfterToolCallFn, BeforeToolCallFn, PromptInput,
	QueueMode, ToolExecutionMode, ToolUpdate, DEFAULT_TOOL_EXECUTION,
};
