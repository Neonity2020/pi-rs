//! Core — the mode-agnostic session layer shared by interactive/print/rpc,
//! port of `pi/packages/coding-agent/src/core/*`.

pub mod agent_session;
pub mod compaction;
pub mod context_usage;
pub mod event_bus;
pub mod export_html;
pub mod json_event;
pub mod mcp_config;
pub mod model_registry;
pub mod retry;
pub mod session_manager;
pub mod settings_manager;
pub mod slash_commands;
pub mod system_prompt;
pub mod tool_selection;
pub mod tools;
pub mod trust;
pub mod ui_config;

pub use session_manager::SessionManager;
