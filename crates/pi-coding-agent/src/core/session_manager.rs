//! Session persistence — port of
//! `pi/packages/coding-agent/src/core/session-manager.ts`.
//!
//! Append-only JSONL: a header line, then one entry per recorded event
//! (`message`, `model-change`, `usage`, ...). The TS original also keeps a
//! branch tree projection for `/tree` navigation; that lands with the
//! session-resume phase.
//!
//! Durability: each entry is written and fsynced (`sync_data`) before
//! `append` returns, so completed appends survive process and machine
//! crashes. The log is ordered but not transactional: a crash mid-write can
//! leave a partial final line. Reopening the session terminates that torn
//! line with a newline, so later appends stay valid, and readers
//! (`read_session_messages`, `count_entries`) skip lines that fail to parse.
//! The TS original does not fsync; this port trades a little latency for
//! that guarantee.

use std::io::{Read, Seek, Write};
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
		message: Box<Message>,
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
	#[error("serialization error: {0}")]
	Serialization(#[from] serde_json::Error),
	#[error("session already has a header")]
	AlreadyInitialized,
	#[error("no session directory (sessions disabled)")]
	Disabled,
}

impl SessionManager {
	/// Create a fresh timestamped session under the sessions root.
	pub fn create(cwd: &Path) -> Result<Self, SessionError> {
		let root = config::sessions_dir();
		let id = format!("{}-{}", chrono_like_timestamp(), std::process::id());
		let dir = root.join(id);
		std::fs::create_dir_all(&dir)?;

		let mut manager = Self {
			session_dir: dir,
			file: None,
			entries_written: 0,
		};
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
		Self {
			session_dir: PathBuf::new(),
			file: None,
			entries_written: 0,
		}
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

	/// Wrap an arbitrary file handle (tests inject failing writers).
	#[cfg(test)]
	pub(crate) fn with_file(dir: PathBuf, file: std::fs::File) -> Self {
		Self {
			session_dir: dir,
			file: Some(file),
			entries_written: 0,
		}
	}

	fn open_file(&mut self) -> Result<(), SessionError> {
		if self.session_dir.as_os_str().is_empty() {
			return Ok(()); // in-memory
		}
		let path = self.session_dir.join("session.jsonl");
		let mut file = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.read(true)
			.open(path)?;
		// Repair a torn final line (crash mid-write) so the next entry starts
		// on a fresh line instead of gluing onto it.
		let len = file.metadata()?.len();
		if len > 0 {
			let mut last = [0u8; 1];
			file.seek(std::io::SeekFrom::End(-1))?;
			file.read_exact(&mut last)?;
			if last[0] != b'\n' {
				file.write_all(b"\n")?;
			}
		}
		self.file = Some(file);
		Ok(())
	}

	pub fn session_dir(&self) -> Option<&Path> {
		if self.session_dir.as_os_str().is_empty() {
			None
		} else {
			Some(&self.session_dir)
		}
	}

	pub fn session_id(&self) -> Option<String> {
		self.session_dir()
			.and_then(|p| p.file_name())
			.map(|n| n.to_string_lossy().into_owned())
	}

	/// Append one entry; no-op for in-memory sessions.
	pub fn append(&mut self, entry: &SessionEntry) -> Result<(), SessionError> {
		let Some(file) = &mut self.file else {
			return Ok(());
		};
		let mut line = serde_json::to_string(entry)?;
		line.push('\n');
		file.write_all(line.as_bytes())?;
		file.sync_data()?;
		self.entries_written += 1;
		Ok(())
	}

	pub fn append_message(&mut self, message: &Message) -> Result<(), SessionError> {
		self.append(&SessionEntry::Message {
			message: Box::new(message.clone()),
		})
	}

	/// List recent sessions (newest first) with their entry counts.
	pub fn list_recent(limit: usize) -> Vec<(PathBuf, u64)> {
		let root = config::sessions_dir();
		let Ok(entries) = std::fs::read_dir(&root) else {
			return Vec::new();
		};
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
	let Ok(text) = std::fs::read_to_string(path) else {
		return Ok(0);
	};
	Ok(text
		.lines()
		.filter(|l| !l.trim().is_empty())
		// A partial line from a crash mid-write is not a recovered entry.
		.filter(|l| serde_json::from_str::<Value>(l).is_ok())
		.count() as u64)
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

	fn temp_session_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-session-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn user_message(text: &str) -> Message {
		Message::User(pi_ai::types::UserMessage::text(text))
	}

	#[test]
	fn open_missing_dir_fails() {
		let dir = std::env::temp_dir().join(format!("pi-rs-missing-{}", pi_ai::types::now_ms()));
		assert!(matches!(
			SessionManager::open(&dir),
			Err(SessionError::Io(_))
		));
	}

	#[test]
	fn resume_appends_after_reopen_preserves_order() {
		let dir = temp_session_dir("resume");
		{
			let mut session = SessionManager::open(&dir).unwrap();
			session.append_message(&user_message("one")).unwrap();
			session.append_message(&user_message("two")).unwrap();
		}
		let mut session = SessionManager::open(&dir).unwrap();
		assert_eq!(session.entries_written, 2);
		session.append_message(&user_message("three")).unwrap();
		assert_eq!(session.entries_written, 3);
		drop(session);

		let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		let texts: Vec<String> = text
			.lines()
			.filter_map(|l| serde_json::from_str::<Value>(l).ok())
			.map(|v| match &v["message"]["content"] {
				Value::String(s) => s.clone(),
				content => content[0]["text"].as_str().unwrap_or_default().to_string(),
			})
			.collect();
		assert_eq!(texts, vec!["one", "two", "three"]);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn partial_trailing_line_is_not_counted() {
		let dir = temp_session_dir("partial");
		let path = dir.join("session.jsonl");
		let mut session = SessionManager::open(&dir).unwrap();
		session.append_message(&user_message("complete")).unwrap();
		drop(session);
		// Simulate a crash mid-write: append half of a JSON line.
		{
			let mut file = std::fs::OpenOptions::new()
				.append(true)
				.open(&path)
				.unwrap();
			use std::io::Write;
			file.write_all(b"{\"type\":\"mess").unwrap();
		}

		let reopened = SessionManager::open(&dir).unwrap();
		assert_eq!(reopened.entries_written, 1);
		// New appends must start on a fresh line, not glue onto the partial one.
		let mut session = reopened;
		session
			.append_message(&user_message("after crash"))
			.unwrap();
		drop(session);
		assert_eq!(
			crate::core::agent_session::read_session_messages(&dir).len(),
			2
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn write_failure_is_reported_and_does_not_advance() {
		let dir = temp_session_dir("iofail");
		let path = dir.join("readonly.jsonl");
		std::fs::write(&path, "").unwrap();
		// A read-only handle fails on the first write (EBADF), like ENOSPC would.
		let readonly = std::fs::File::open(&path).unwrap();
		let mut session = SessionManager::with_file(dir.clone(), readonly);
		assert!(matches!(
			session.append_message(&user_message("x")),
			Err(SessionError::Io(_))
		));
		assert_eq!(session.entries_written, 0);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn in_memory_append_never_fails_and_writes_nothing() {
		let mut session = SessionManager::in_memory();
		session.append_message(&user_message("ghost")).unwrap();
		assert_eq!(session.entries_written, 0);
		assert!(session.session_dir().is_none());
	}
}
