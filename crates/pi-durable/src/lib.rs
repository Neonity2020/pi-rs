//! pi-durable — durable conversation, task, and document runtime for pi.
//!
//! Rust port of `pi/packages/durable`: JSONL/SQLite storage backends, durable
//! effects with replay policy (see `AgentTool::replay` in pi-agent), and the
//! task/document runtimes built on top.
//!
//! Framework stage: placeholder. The JSONL storage backend reuses
//! pi-coding-agent's `session_manager` logic; extracting it into this crate
//! happens with the durable phase.
