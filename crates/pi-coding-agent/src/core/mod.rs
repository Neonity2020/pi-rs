//! Core — the mode-agnostic session layer shared by interactive/print/rpc,
//! port of `pi/packages/coding-agent/src/core/*`.

pub mod agent_session;
pub mod auth_storage;
pub mod changelog;
pub mod clipboard;
pub mod compaction;
pub mod context_usage;
pub mod diagnostics;
pub mod event_bus;
pub mod export_html;
pub mod extensions;
pub mod frontmatter;
pub mod image_process;
pub mod json_event;
pub mod mcp_config;
pub mod model_registry;
pub mod open_browser;
pub mod package_manager;
pub mod prompt_templates;
pub mod retry;
pub mod session_export;
pub mod session_manager;
pub mod settings_manager;
pub mod skills;
pub mod slash_commands;
pub mod system_prompt;
pub mod tool_selection;
pub mod tools;
pub mod trust;
pub mod ui_config;
