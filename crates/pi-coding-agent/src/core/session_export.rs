//! Session JSONL export — port of `pi/packages/coding-agent/src/core/
//! session-export.ts` (`serializeSessionBranch` / `exportSessionToJsonl`):
//! the current root-to-leaf branch with re-chained `parentId` links under a
//! fresh export header. `/export <path>.jsonl` in the interactive mode and
//! upstream's `session.exportToJsonl` share this.

use std::path::{Path, PathBuf};

use super::session_manager::{CURRENT_SESSION_VERSION, SessionManager};

/// Serialize the current branch: a fresh `type:"session"` header (new
/// timestamp, same id/cwd), then the branch entries with `parentId` re-chained
/// along the path.
pub fn serialize_session_branch(manager: &SessionManager) -> String {
	let timestamp = super::session_manager::iso_timestamp(pi_ai::types::now_ms());
	let header = super::session_manager::SessionHeader {
		version: Some(CURRENT_SESSION_VERSION),
		id: manager.header_id().unwrap_or_default().to_string(),
		timestamp: timestamp.clone(),
		cwd: manager
			.header()
			.map(|header| header.cwd.clone())
			.unwrap_or_default(),
		parent_session: manager
			.header()
			.and_then(|header| header.parent_session.clone()),
	};
	let mut lines = vec![header.to_value().to_string()];
	let branch = super::session_manager::build_session_path(manager.entries(), manager.leaf_id());
	let mut parent_id: Option<String> = None;
	for entry in branch {
		let mut value = entry.to_value();
		if let Some(parent) = &parent_id {
			value["parentId"] = serde_json::Value::String(parent.clone());
		} else {
			value["parentId"] = serde_json::Value::Null;
		}
		let id = entry.id().map(str::to_string);
		parent_id = id;
		lines.push(value.to_string());
	}
	let mut out = lines.join("\n");
	out.push('\n');
	out
}

/// Write the current branch as JSONL. The default file name is
/// `session-<ISO timestamp>.jsonl` resolved against `cwd` (upstream
/// `exportSessionToJsonl`); the parent directory is created when missing.
/// Returns the written path.
pub fn export_session_to_jsonl(
	manager: &SessionManager,
	output_path: Option<&Path>,
	cwd: &Path,
) -> anyhow::Result<PathBuf> {
	let default_name = format!(
		"session-{}.jsonl",
		super::session_manager::iso_timestamp(pi_ai::types::now_ms()).replace([':', '.'], "-")
	);
	let file_path = match output_path {
		Some(path) => {
			if path.is_absolute() {
				path.to_path_buf()
			} else {
				cwd.join(path)
			}
		}
		None => cwd.join(default_name),
	};
	if let Some(parent) = file_path.parent()
		&& !parent.as_os_str().is_empty()
		&& !parent.exists()
	{
		std::fs::create_dir_all(parent)?;
	}
	std::fs::write(&file_path, serialize_session_branch(manager))?;
	Ok(file_path)
}

pub fn import_session_from_jsonl(path: &Path, root: &Path, cwd: &Path) -> Result<PathBuf, String> {
	let text = std::fs::read_to_string(path)
		.map_err(|error| format!("Could not read file {path:?}: {error}"))?;
	let mut lines = text.lines().filter(|line| !line.trim().is_empty());
	let header_line = lines
		.next()
		.ok_or_else(|| format!("Not a session export (empty file): {path:?}"))?;
	let header_value: serde_json::Value = serde_json::from_str(header_line)
		.map_err(|error| format!("Not a session export (bad header): {error}"))?;
	if header_value.get("type").and_then(serde_json::Value::as_str) != Some("session") {
		return Err(format!(
			"Not a session export (missing session header): {path:?}"
		));
	}
	let entries: Vec<crate::core::session_manager::SessionEntry> = lines
		.map(|line| {
			let value: serde_json::Value = serde_json::from_str(line)
				.map_err(|error| format!("Malformed entry in {path:?}: {error}"))?;
			crate::core::session_manager::SessionEntry::from_value(&value)
				.ok_or_else(|| format!("Unrecognized entry in {path:?}"))
		})
		.collect::<Result<_, _>>()?;

	let mut manager =
		crate::core::session_manager::SessionManager::create_with_parent_in(root, cwd, None)
			.map_err(|error| format!("Could not create the import directory: {error}"))?;
	for entry in entries {
		manager
			.append(entry)
			.map_err(|error| format!("Import failed while writing: {error}"))?;
	}
	Ok(manager
		.session_dir()
		.map(Path::to_path_buf)
		.unwrap_or_else(|| root.to_path_buf()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::core::session_manager::SessionEntry;
	use pi_ai::Message;

	fn temp_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-export-jsonl-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn serialized_branch_rechains_parent_ids_under_a_fresh_header() {
		let dir = temp_dir("serialize");
		let mut manager =
			SessionManager::create_with_parent_in(&dir, Path::new("/tmp/work"), None).unwrap();
		manager
			.append(SessionEntry::new(
				super::super::session_manager::EntryKind::Message {
					message: Box::new(Message::user("one")),
				},
			))
			.unwrap();
		manager
			.append(SessionEntry::new(
				super::super::session_manager::EntryKind::Message {
					message: Box::new(Message::user("two")),
				},
			))
			.unwrap();

		let serialized = serialize_session_branch(&manager);
		let lines: Vec<serde_json::Value> = serialized
			.lines()
			.map(|line| serde_json::from_str(line).unwrap())
			.collect();
		assert_eq!(lines[0]["type"], "session", "header first");
		assert_eq!(lines[0]["cwd"], "/tmp/work");
		assert_eq!(lines[1]["parentId"], serde_json::Value::Null, "root entry");
		assert_eq!(lines[2]["parentId"], lines[1]["id"], "chained");
		// The entries are the same messages in branch order.
		assert_eq!(lines[1]["message"]["content"][0]["text"], "one");
		assert_eq!(lines[2]["message"]["content"][0]["text"], "two");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn export_writes_default_and_explicit_paths() {
		let dir = temp_dir("export");
		let cwd = dir.join("cwd");
		std::fs::create_dir_all(&cwd).unwrap();
		let manager =
			SessionManager::create_with_parent_in(&dir, Path::new("/tmp/work"), None).unwrap();

		// Default: session-<timestamp>.jsonl next to the cwd.
		let path = export_session_to_jsonl(&manager, None, &cwd).unwrap();
		assert!(path.starts_with(&cwd));
		assert!(
			path.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with("session-"))
		);
		assert!(path.exists());

		// Explicit relative path resolves against the cwd; nested parents are
		// created.
		let path =
			export_session_to_jsonl(&manager, Some(Path::new("out/log.jsonl")), &cwd).unwrap();
		assert_eq!(path, cwd.join("out/log.jsonl"));
		assert!(path.exists());
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn import_round_trips_an_export_into_a_fresh_directory() {
		let dir = temp_dir("import");
		let cwd = dir.join("cwd");
		std::fs::create_dir_all(&cwd).unwrap();
		let mut manager =
			SessionManager::create_with_parent_in(&dir, Path::new("/tmp/work"), None).unwrap();
		manager
			.append(SessionEntry::new(
				crate::core::session_manager::EntryKind::Message {
					message: Box::new(Message::user("exported one")),
				},
			))
			.unwrap();
		let export_path = export_session_to_jsonl(&manager, None, &cwd).unwrap();
		let exported_header_id = crate::core::session_manager::build_session_info(&export_path)
			.unwrap()
			.id;

		let imported_dir =
			import_session_from_jsonl(&export_path, &dir, Path::new("/tmp/other")).unwrap();
		let (header, entries) =
			crate::core::session_manager::load_session_file(&imported_dir.join("session.jsonl"));
		let header = header.expect("valid imported header");
		assert_ne!(header.id, exported_header_id, "fresh session id on import");
		assert_eq!(header.cwd, "/tmp/other");
		assert_eq!(entries.len(), 1, "the exported entry survives");
		let entry_value = entries[0].to_value();
		let text = entry_value
			.pointer("/message/content/0/text")
			.and_then(serde_json::Value::as_str)
			.unwrap_or_default();
		assert_eq!(text, "exported one");
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
