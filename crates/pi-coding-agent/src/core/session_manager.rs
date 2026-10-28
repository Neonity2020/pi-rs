//! Session persistence — port of
//! `pi/packages/coding-agent/src/core/session-manager.ts`.
//!
//! Append-only JSONL: a header line, then one entry per recorded event
//! (`message`, `model-change`, `usage`, ...). The TS original also keeps a
//! branch tree projection for `/tree` navigation; that lands with the
//! session-resume phase.
//!
//! Durability: each entry is written and fsynced (`sync_data`) before
//! `append` returns. Filesystem and hardware durability guarantees vary;
//! the log is ordered but not transactional: a crash mid-write can
//! leave a partial final line. Reopening the session terminates that torn
//! line with a newline, so later appends stay valid, and readers
//! (`read_session_messages`, `count_entries`) skip lines that fail to parse.
//! The TS original does not fsync; this port trades latency for a sync request.

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
		#[serde(skip_serializing_if = "Option::is_none")]
		id: Option<String>,
		message: Box<Message>,
	},
	ModelChange {
		model: String,
	},
	/// Compaction boundary — upstream `CompactionEntry`
	/// (`pi/packages/coding-agent/src/core/session-manager.ts`). This flat-log
	/// port summarizes the whole non-system transcript ("keep nothing"), so
	/// `firstKeptEntryId` mirrors upstream's `firstKeptEntryId ?? id`: it
	/// points at the compaction entry itself. Upstream's optional fields
	/// (`details`, `usage`, `fromHook`, `systemMessage`) are not produced yet;
	/// their absence stays wire-compatible. The variant-level `rename_all`
	/// covers its multi-word fields (the container attribute names variants).
	#[serde(rename_all = "camelCase")]
	Compaction {
		id: String,
		summary: String,
		first_kept_entry_id: String,
		tokens_before: u64,
	},
	/// Persistently omit a failed assistant response from the context.
	#[serde(rename = "context_edit", rename_all = "camelCase")]
	ContextEdit {
		target_id: String,
		replacement: Option<Value>,
	},
	Custom {
		kind: String,
		data: Value,
	},
}

impl SessionEntry {
	/// Build a compaction entry with a fresh unique id (upstream
	/// `SessionManager.appendCompaction`, reduced to the keep-nothing form).
	pub fn compaction(summary: impl Into<String>, tokens_before: u64) -> Self {
		let id = generate_entry_id();
		Self::Compaction {
			id: id.clone(),
			summary: summary.into(),
			first_kept_entry_id: id,
			tokens_before,
		}
	}
}

/// Time-plus-counter entry id (upstream `generateId`); only uniqueness within
/// one log matters to this port.
fn generate_entry_id() -> String {
	use std::sync::atomic::{AtomicU64, Ordering};
	static COUNTER: AtomicU64 = AtomicU64::new(0);
	let n = COUNTER.fetch_add(1, Ordering::Relaxed);
	format!("{:x}-{:x}", pi_ai::types::now_ms(), n)
}

pub struct SessionManager {
	session_dir: PathBuf,
	file: Option<std::fs::File>,
	pending_sync: Option<String>,
	pending_metadata: bool,
	pending_message_id: Option<String>,
	#[cfg(test)]
	fail_next_sync: bool,
	write_broken: bool,
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
			pending_sync: None,
			pending_metadata: false,
			pending_message_id: None,
			#[cfg(test)]
			fail_next_sync: false,
			write_broken: false,
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
			pending_sync: None,
			pending_metadata: false,
			pending_message_id: None,
			#[cfg(test)]
			fail_next_sync: false,
			write_broken: false,
			entries_written: 0,
		}
	}

	/// Open an existing session directory for append.
	pub fn open(dir: &Path) -> Result<Self, SessionError> {
		let mut manager = Self {
			session_dir: dir.to_path_buf(),
			file: None,
			pending_sync: None,
			pending_metadata: false,
			pending_message_id: None,
			#[cfg(test)]
			fail_next_sync: false,
			write_broken: false,
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
			pending_sync: None,
			pending_metadata: false,
			pending_message_id: None,
			#[cfg(test)]
			fail_next_sync: false,
			write_broken: false,
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
		#[cfg(test)]
		if std::mem::take(&mut self.fail_next_sync) {
			return self.append_with_io(
				entry,
				|file, bytes| file.write_all(bytes),
				|_| Err(std::io::Error::other("injected sync failure")),
			);
		}
		self.append_with_io(
			entry,
			|file, bytes| file.write_all(bytes),
			|file| file.sync_data(),
		)
	}

	fn append_with_io(
		&mut self,
		entry: &SessionEntry,
		write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
		sync: impl FnOnce(&std::fs::File) -> std::io::Result<()>,
	) -> Result<(), SessionError> {
		let Some(file) = &mut self.file else {
			return Ok(());
		};
		let mut line = serde_json::to_string(entry)?;
		line.push('\n');
		if self.write_broken {
			return Err(std::io::Error::other(
				"session log requires reopening after failed rollback",
			)
			.into());
		}
		if let Some(pending) = &self.pending_sync {
			if pending != &line {
				return Err(
					std::io::Error::other("previous session entry still awaiting sync").into(),
				);
			}
		} else {
			let original_len = file.metadata()?.len();
			if let Err(error) = write(file, line.as_bytes()) {
				if file.set_len(original_len).is_err() {
					self.write_broken = true;
				}
				return Err(error.into());
			}
			self.pending_sync = Some(line);
			self.pending_metadata = !matches!(entry, SessionEntry::Message { .. });
		}
		sync(file)?;
		self.pending_sync = None;
		self.pending_metadata = false;
		self.entries_written += 1;
		Ok(())
	}

	pub fn append_message(&mut self, message: &Message) -> Result<(), SessionError> {
		let id = self
			.pending_message_id
			.get_or_insert_with(generate_entry_id)
			.clone();
		let result = self.append(&SessionEntry::Message {
			id: Some(id),
			message: Box::new(message.clone()),
		});
		if result.is_ok() {
			self.pending_message_id = None;
		}
		result
	}

	#[cfg(test)]
	pub(crate) fn fail_next_sync(&mut self) {
		self.fail_next_sync = true;
	}

	/// Finish a previously written metadata entry before appending messages.
	pub fn sync_pending_metadata(&mut self) -> Result<(), SessionError> {
		if self.pending_metadata {
			if let Some(file) = &self.file {
				file.sync_data()?;
			}
			self.pending_sync = None;
			self.pending_metadata = false;
			self.entries_written += 1;
		}
		Ok(())
	}

	/// List recent sessions (newest first) with their entry counts.
	pub fn list_recent(limit: usize) -> Vec<(PathBuf, u64)> {
		list_recent_in(&config::sessions_dir(), limit)
	}
}

/// Which session a resume flag names — mirrors the `--continue`/`--resume`
/// selection in `pi/packages/coding-agent/src/main.ts` (`parsed.continue` /
/// `parsed.resume` + `resolveSessionPath`), reduced to this port's flat
/// sessions root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelector {
	/// `--continue`: the most recent session.
	Recent,
	/// `--resume <id|path>`: a session id (directory name under the sessions
	/// root, exact or unique prefix) or a path to the session directory.
	ByIdOrPath(String),
}

/// Locate the session directory a resume selector names.
pub fn resolve_session(selector: &SessionSelector) -> Result<PathBuf, String> {
	resolve_session_in(&config::sessions_dir(), selector)
}

/// Root-parameterized [`resolve_session`] (tests inject a temporary root).
pub fn resolve_session_in(root: &Path, selector: &SessionSelector) -> Result<PathBuf, String> {
	match selector {
		SessionSelector::Recent => list_recent_in(root, 1)
			.into_iter()
			.next()
			.map(|(dir, _)| dir)
			.ok_or_else(|| "no previous session to continue".to_string()),
		SessionSelector::ByIdOrPath(arg) => {
			let candidate = PathBuf::from(arg);
			if candidate.is_dir() {
				return Ok(candidate);
			}
			// Exact id (directory name), then a unique prefix match.
			let exact = root.join(arg);
			if exact.is_dir() {
				return Ok(exact);
			}
			let matches = prefix_matches(root, arg);
			match matches.as_slice() {
				[only] => Ok(only.clone()),
				[] => Err(format!("no session found matching `{arg}`")),
				many => Err(format!(
					"session id `{arg}` is ambiguous, {} matches: {}",
					many.len(),
					many.iter()
						.filter_map(|p| p.file_name())
						.map(|n| n.to_string_lossy().into_owned())
						.collect::<Vec<_>>()
						.join(", ")
				)),
			}
		}
	}
}

/// Validate a session directory before reopening it, so a missing or corrupt
/// session fails loudly instead of silently starting an empty log.
pub fn validate_session_dir(dir: &Path) -> Result<(), String> {
	let path = dir.join("session.jsonl");
	let text = std::fs::read_to_string(&path)
		.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
	for line in text.lines() {
		let line = line.trim();
		if line.is_empty() {
			continue;
		}
		let Ok(value) = serde_json::from_str::<Value>(line) else {
			break; // torn header line: nothing recoverable before it
		};
		if value.get("type").and_then(Value::as_str) == Some("header") {
			return Ok(());
		}
		break;
	}
	Err(format!(
		"{} is not a valid session directory (no session header)",
		dir.display()
	))
}

fn list_recent_in(root: &Path, limit: usize) -> Vec<(PathBuf, u64)> {
	let Ok(entries) = std::fs::read_dir(root) else {
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

fn prefix_matches(root: &Path, prefix: &str) -> Vec<PathBuf> {
	let Ok(entries) = std::fs::read_dir(root) else {
		return Vec::new();
	};
	let mut matches: Vec<PathBuf> = entries
		.flatten()
		.map(|e| e.path())
		.filter(|p| {
			p.is_dir()
				&& p.file_name()
					.is_some_and(|n| n.to_string_lossy().starts_with(prefix))
		})
		.collect();
	matches.sort();
	matches
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
	fn retry_after_sync_failure_does_not_append_twice() {
		let dir = temp_session_dir("sync-retry");
		let mut session = SessionManager::open(&dir).unwrap();
		let entry = SessionEntry::Message {
			id: None,
			message: Box::new(user_message("once")),
		};
		assert!(
			session
				.append_with_io(
					&entry,
					|f, bytes| f.write_all(bytes),
					|_| { Err(std::io::Error::other("injected sync failure")) }
				)
				.is_err()
		);
		assert!(session.append_message(&user_message("different")).is_err());
		session.append(&entry).unwrap();
		assert_eq!(session.entries_written, 1);
		assert_eq!(
			crate::core::agent_session::read_session_messages(&dir).len(),
			1
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn partial_write_failure_rolls_back_before_retry() {
		let dir = temp_session_dir("partial-write");
		let mut session = SessionManager::open(&dir).unwrap();
		session.append_message(&user_message("before")).unwrap();
		let entry = SessionEntry::Message {
			id: None,
			message: Box::new(user_message("after")),
		};
		assert!(
			session
				.append_with_io(
					&entry,
					|file, bytes| {
						file.write_all(&bytes[..bytes.len() / 2])?;
						Err(std::io::Error::other("injected partial write"))
					},
					|file| file.sync_data()
				)
				.is_err()
		);
		assert_eq!(session.entries_written, 1);
		session.append(&entry).unwrap();
		let messages = crate::core::agent_session::read_session_messages(&dir);
		assert_eq!(messages.len(), 2);
		let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(text.lines().count(), 2);
		assert!(
			text.lines()
				.all(|line| serde_json::from_str::<Value>(line).is_ok())
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn failed_metadata_sync_can_be_finished_before_message() {
		let dir = temp_session_dir("metadata-sync");
		let mut session = SessionManager::open(&dir).unwrap();
		let change = SessionEntry::ModelChange {
			model: "test/model".into(),
		};
		assert!(
			session
				.append_with_io(
					&change,
					|file, bytes| file.write_all(bytes),
					|_| { Err(std::io::Error::other("injected sync failure")) }
				)
				.is_err()
		);
		session.sync_pending_metadata().unwrap();
		session.append_message(&user_message("after")).unwrap();
		let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(text.lines().count(), 2);
		assert_eq!(session.entries_written, 2);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn in_memory_append_never_fails_and_writes_nothing() {
		let mut session = SessionManager::in_memory();
		session.append_message(&user_message("ghost")).unwrap();
		assert_eq!(session.entries_written, 0);
		assert!(session.session_dir().is_none());
	}

	// -- compaction entry ----------------------------------------------------

	#[test]
	fn compaction_entry_serializes_camel_case() {
		let entry = SessionEntry::compaction("the summary", 1234);
		let value = serde_json::to_value(&entry).unwrap();
		assert_eq!(value["type"], "compaction");
		assert_eq!(value["summary"], "the summary");
		assert_eq!(value["tokensBefore"], 1234);
		// Keep-nothing form: the entry keeps itself (upstream `firstKeptEntryId ?? id`).
		assert_eq!(value["firstKeptEntryId"], value["id"]);
		assert!(!value["id"].as_str().unwrap().is_empty());
	}

	#[test]
	fn compaction_entry_ids_are_unique() {
		let first = serde_json::to_value(SessionEntry::compaction("a", 1)).unwrap();
		let second = serde_json::to_value(SessionEntry::compaction("b", 2)).unwrap();
		assert_ne!(first["id"], second["id"]);
	}

	#[test]
	fn existing_entry_shapes_are_stable() {
		let header = serde_json::to_value(&SessionEntry::Header {
			version: 1,
			app: "pi-rs 0.1.0".into(),
			created: 42,
			cwd: "/tmp".into(),
		})
		.unwrap();
		assert_eq!(header["type"], "header");
		let change = serde_json::to_value(&SessionEntry::ModelChange {
			model: "mock/mock".into(),
		})
		.unwrap();
		assert_eq!(change["type"], "modelChange");
		assert_eq!(change["model"], "mock/mock");
		let message = serde_json::to_value(&SessionEntry::Message {
			id: None,
			message: Box::new(user_message("hi")),
		})
		.unwrap();
		assert_eq!(message["type"], "message");
		assert_eq!(message["message"]["role"], "user");
	}

	// -- resume selection ---------------------------------------------------

	fn resume_root(tag: &str) -> PathBuf {
		let root = std::env::temp_dir().join(format!(
			"pi-rs-resolve-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&root).unwrap();
		root
	}

	fn make_session(root: &Path, id: &str, text: &str) -> PathBuf {
		let dir = root.join(id);
		std::fs::create_dir_all(&dir).unwrap();
		let mut session = SessionManager::open(&dir).unwrap();
		session
			.append(&SessionEntry::Header {
				version: 1,
				app: format!("{} {}", crate::config::APP_NAME, crate::config::VERSION),
				created: pi_ai::types::now_ms(),
				cwd: root.display().to_string(),
			})
			.unwrap();
		session.append_message(&user_message(text)).unwrap();
		dir
	}

	#[test]
	fn resolve_recent_picks_newest() {
		let root = resume_root("recent");
		make_session(&root, "20260101-000000-1", "old");
		make_session(&root, "20260303-000000-3", "new");
		make_session(&root, "20260202-000000-2", "mid");
		let dir = resolve_session_in(&root, &SessionSelector::Recent).unwrap();
		assert_eq!(dir.file_name().unwrap(), "20260303-000000-3");
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_recent_without_sessions_errors() {
		let root = resume_root("empty");
		assert_eq!(
			resolve_session_in(&root, &SessionSelector::Recent),
			Err("no previous session to continue".to_string())
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_by_exact_id_and_prefix() {
		let root = resume_root("byid");
		make_session(&root, "20260101-000000-1", "one");
		let target = make_session(&root, "20260202-000000-2", "two");
		assert_eq!(
			resolve_session_in(
				&root,
				&SessionSelector::ByIdOrPath("20260202-000000-2".into())
			)
			.unwrap(),
			target
		);
		// Unique prefix match.
		assert_eq!(
			resolve_session_in(&root, &SessionSelector::ByIdOrPath("20260202".into())).unwrap(),
			target
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_by_path_beats_id_lookup() {
		let root = resume_root("bypath");
		let target = make_session(&root, "20260101-000000-1", "one");
		assert_eq!(
			resolve_session_in(
				&root,
				&SessionSelector::ByIdOrPath(target.display().to_string())
			)
			.unwrap(),
			target
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_errors_are_specific() {
		let root = resume_root("errors");
		make_session(&root, "20260101-000000-1", "one");
		make_session(&root, "20260101-000000-11", "eleven");
		assert!(
			resolve_session_in(&root, &SessionSelector::ByIdOrPath("nope".into()))
				.unwrap_err()
				.contains("no session found matching")
		);
		// Ambiguous prefix (not an exact id) across two sessions.
		assert!(
			resolve_session_in(&root, &SessionSelector::ByIdOrPath("20260101".into()))
				.unwrap_err()
				.contains("ambiguous")
		);
		// An exact id still wins over a multi-match prefix.
		assert!(
			resolve_session_in(
				&root,
				&SessionSelector::ByIdOrPath("20260101-000000-1".into())
			)
			.is_ok()
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn validation_rejects_missing_or_headerless_dirs() {
		let root = resume_root("validate");
		let missing = root.join("missing");
		assert!(
			validate_session_dir(&missing)
				.unwrap_err()
				.contains("cannot read")
		);

		let corrupt = root.join("corrupt");
		std::fs::create_dir_all(&corrupt).unwrap();
		std::fs::write(corrupt.join("session.jsonl"), "{\"type\":\"message\"}\n").unwrap();
		assert!(
			validate_session_dir(&corrupt)
				.unwrap_err()
				.contains("no session header")
		);

		let good = make_session(&root, "20260101-000000-1", "one");
		assert!(validate_session_dir(&good).is_ok());
		std::fs::remove_dir_all(root).unwrap();
	}
}
