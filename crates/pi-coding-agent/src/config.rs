//! App identity and on-disk layout — port of
//! `pi/packages/coding-agent/src/config.ts`.
//!
//! pi stores everything under `~/.pi/agent`; pi-rs uses `~/.pi-rs/agent`
//! (override: `PI_RS_AGENT_DIR`) so both agents can coexist on one machine.

use std::path::PathBuf;

pub const APP_NAME: &str = "pi-rs";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Env var overriding the agent directory.
pub const AGENT_DIR_ENV: &str = "PI_RS_AGENT_DIR";
/// Env var overriding the sessions root.
pub const SESSION_DIR_ENV: &str = "PI_RS_SESSION_DIR";

/// `~/.pi-rs/agent` (or `$PI_RS_AGENT_DIR`).
pub fn agent_dir() -> PathBuf {
	if let Some(dir) = std::env::var_os(AGENT_DIR_ENV) {
		return PathBuf::from(dir);
	}
	dirs::home_dir().unwrap_or_default().join(".pi-rs").join("agent")
}

pub fn sessions_dir() -> PathBuf {
	if let Some(dir) = std::env::var_os(SESSION_DIR_ENV) {
		return PathBuf::from(dir);
	}
	agent_dir().join("sessions")
}

pub fn models_json_path() -> PathBuf {
	agent_dir().join("models.json")
}

pub fn auth_json_path() -> PathBuf {
	agent_dir().join("auth.json")
}

pub fn settings_json_path() -> PathBuf {
	agent_dir().join("settings.json")
}

pub fn themes_dir() -> PathBuf {
	agent_dir().join("themes")
}

pub fn prompts_dir() -> PathBuf {
	agent_dir().join("prompts")
}

pub fn skills_dir() -> PathBuf {
	agent_dir().join("skills")
}

pub fn debug_log_path() -> PathBuf {
	agent_dir().join("pi-rs-debug.log")
}

/// Project-local pi directory (`.pi`) for project settings/skills/prompts.
pub fn project_pi_dir(cwd: &std::path::Path) -> PathBuf {
	cwd.join(".pi")
}
