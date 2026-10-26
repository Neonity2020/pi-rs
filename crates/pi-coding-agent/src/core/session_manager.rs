//! Session persistence — port of
//! `pi/packages/coding-agent/src/core/session-manager.ts`.
//!
//! Append-only JSONL: a header line, then one entry per recorded event
//! (`message`, `model-change`, `usage`, ...). The TS original also keeps a
//! branch tree projection for `/tree` navigation; that lands with the
//! session-resume phase.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use pi_ai::Message;

use crate::config;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionEntry {
	Header {
		version: u32,
		app: String,
		created: u64,
		cwd: String,
	},
	Message {
		message: Message,
	},
	ModelChange {
		model: String,
	},
	Custom {
		kind: String,
		data: Value,
	},
}

pub struct SessionManager {
	session_dir: PathBuf,
	file: Option<std::fs::File>,
	pub entries_written: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
	#[error("io error: {0}")]
	Io(#[from] std::io::Error),
	#[error("session already has a header")]
	AlreadyInitialized,
	#[error("no session directory (sessions disabled)")]
	Disabled,
}

impl SessionManager {
	/// Create a fresh timestamped session under the sessions root.
	pub fn create(cwd: &Path) -> Result<Self, SessionError> {
		let root = config::sessions_dir();
		let id = format!(
			"{}-{}",
			chrono_like_timestamp(),
			std::process::id()
		);
		let dir = root.join(id);
		std::fs::create_dir_all(&dir)?;

		let mut manager = Self { session_dir: dir, file: None, entries_written: 0 };
		manager.open_file()?;
		manager.append(&SessionEntry::Header {
			version: 1,
			app: format!("{} {}", crate::config::APP_NAME, crate::config::VERSION),
			created: pi_ai::types::now_ms(),
			cwd: cwd.to_string_lossy().into_owned(),
		})?;
		Ok(manager)
	}

	/// In-memory session (persistence disabled, `--no-session`).
	pub fn in_memory() -> Self {
		Self { session_dir: PathBuf::new(), file: None, entries_written: 0 }
	}

	/// Open an existing session directory for append.
	pub fn open(dir: &Path) -> Result<Self, SessionError> {
		let mut manager = Self {
			session_dir: dir.to_path_buf(),
			file: None,
			entries_written: count_entries(dir)?,
		};
		manager.open_file()?;
		Ok(manager)
	}

	fn open_file(&mut self) -> Result<(), SessionError> {
		if self.session_dir.as_os_str().is_empty() {
			return Ok(()); // in-memory
		}
		let path = self.session_dir.join("session.jsonl");
		let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
		self.file = Some(file);
		Ok(())
	}

	pub fn session_dir(&self) -> Option<&Path> {
		if self.session_dir.as_os_str().is_empty() { None } else { Some(&self.session_dir) }
	}

	pub fn session_id(&self) -> Option<String> {
		self.session_dir()
			.and_then(|p| p.file_name())
			.map(|n| n.to_string_lossy().into_owned())
	}

	/// Append one entry; no-op for in-memory sessions.
	pub fn append(&mut self, entry: &SessionEntry) -> Result<(), SessionError> {
		let Some(file) = &mut self.file else { return Ok(()) };
		let mut line = serde_json::to_string(entry).unwrap_or_else(|_| "{}".into());
		line.push('\n');
		file.write_all(line.as_bytes())?;
		self.entries_written += 1;
		Ok(())
	}

	pub fn append_message(&mut self, message: &Message) -> Result<(), SessionError> {
		self.append(&SessionEntry::Message { message: message.clone() })
	}

	/// List recent sessions (newest first) with their entry counts.
	pub fn list_recent(limit: usize) -> Vec<(PathBuf, u64)> {
		let root = config::sessions_dir();
		let Ok(entries) = std::fs::read_dir(&root) else { return Vec::new() };
		let mut sessions: Vec<(PathBuf, u64)> = entries
			.flatten()
			.filter(|e| e.path().is_dir())
			.map(|e| {
				let entries = count_entries(&e.path()).unwrap_or(0);
				(e.path(), entries)
			})
			.collect();
		sessions.sort_by(|a, b| b.0.cmp(&a.0));
		sessions.truncate(limit);
		sessions
	}
}

fn count_entries(dir: &Path) -> Result<u64, SessionError> {
	let path = dir.join("session.jsonl");
	let Ok(text) = std::fs::read_to_string(path) else { return Ok(0) };
	Ok(text.lines().filter(|l| !l.trim().is_empty()).count() as u64)
}

/// `YYYYMMDD-HHMMSS` without pulling in a time crate.
fn chrono_like_timestamp() -> String {
	let ms = pi_ai::types::now_ms();
	let secs = ms / 1000;
	let days = secs / 86400;
	let rem = secs % 86400;
	let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
	// Days since epoch → civil date (Howard Hinnant's algorithm, simplified).
	let z = days as i64 + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let mth = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if mth <= 2 { y + 1 } else { y };
	format!("{y:04}{mth:02}{d:02}-{h:02}{m:02}{s:02}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn timestamp_shape() {
		let ts = chrono_like_timestamp();
		assert_eq!(ts.len(), 15);
		assert!(ts.contains('-'));
	}
}
