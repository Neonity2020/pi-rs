//! Project trust — port of `pi/packages/coding-agent/src/core/trust-manager.ts`
//! (the `ProjectTrustStore`) plus the terminal prompt cut of
//! `pi/packages/coding-agent/src/core/project-trust.ts`, reduced to what
//! pi-rs gates today: the project-level `.pi/mcp.json`.
//!
//! Decisions live in `<agent-dir>/trust.json` as
//! `{ "<canonical path>": true | false | null }` — sorted keys, 2-space
//! indent, trailing newline, matching upstream `writeTrustFile`. A lookup
//! walks up from the working directory to the nearest recorded ancestor
//! (upstream `findNearestTrustEntry`); `null` entries only clear a decision
//! and never match. Differences from upstream: no cross-process lockfile
//! (upstream `proper-lockfile`; pi-rs relies on single-user whole-file
//! writes), and an unreadable or invalid store counts as *untrusted* for
//! lookups instead of throwing, because trust gates code execution — writing
//! still surfaces the error.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;

/// `<agent-dir>/trust.json` — upstream `ProjectTrustStore`.
pub fn trust_json_path() -> PathBuf {
	crate::config::agent_dir().join("trust.json")
}

/// Whether the project at `cwd` — or its nearest recorded ancestor — is
/// trusted. A missing or unreadable store is untrusted.
pub fn is_trusted(cwd: &Path) -> bool {
	is_trusted_at(&trust_json_path(), cwd)
}

/// The project-trust decision point with a CLI override — the pi-rs cut of
/// upstream `projectTrusted` (`main.ts:741-770`), where
/// `parsed.projectTrustOverride` (from `-a`/`--approve` and
/// `-na`/`--no-approve`) short-circuits both the recorded decision and the
/// interactive prompt for this run: `-a` trusts the project's local files
/// (project `.pi/mcp.json` loads), `-na` treats it as untrusted (project
/// resources are skipped with a notice). Without an override the stored
/// decision applies. The override never writes the trust store.
pub fn resolve_project_trusted(cwd: &Path, project_trust_override: Option<bool>) -> bool {
	project_trust_override.unwrap_or_else(|| is_trusted(cwd))
}

/// Record `cwd` as trusted (path normalized first) — upstream `set(cwd, true)`.
pub fn trust(cwd: &Path) -> io::Result<()> {
	trust_at(&trust_json_path(), cwd)
}

/// Record an explicit trust decision for `cwd` — upstream `set(cwd, decision)`
/// (`/trust` selector). Path normalized first.
pub fn set(cwd: &Path, decision: bool) -> io::Result<()> {
	set_decision(&trust_json_path(), cwd, decision)
}

/// Clear the decision recorded exactly for `cwd` (upstream `set(cwd, null)` —
/// the store keeps a `null` entry so the nearest recorded ancestor decides).
pub fn clear(cwd: &Path) -> io::Result<()> {
	let path = trust_json_path();
	let mut store = read_store_checked(&path)?;
	store.insert(normalize_path(cwd).to_string_lossy().into_owned(), None);
	write_store(&path, &store)
}

/// The nearest recorded bool decision for `cwd` with the path it was recorded
/// under — upstream `getEntry` (`ProjectTrustStoreEntry`). `None` when no
/// ancestor has a decision.
pub fn saved_entry(cwd: &Path) -> Option<(PathBuf, bool)> {
	saved_entry_at(&trust_json_path(), cwd)
}

pub(crate) fn saved_entry_at(store: &Path, cwd: &Path) -> Option<(PathBuf, bool)> {
	nearest_entry(&read_store(store), cwd)
}

pub(crate) fn is_trusted_at(store: &Path, cwd: &Path) -> bool {
	matches!(nearest_decision(&read_store(store), cwd), Some(true))
}

pub(crate) fn trust_at(store: &Path, cwd: &Path) -> io::Result<()> {
	set_decision(store, cwd, true)
}

/// The parsed store: path → decision, with `null` clearing a previous entry.
type Store = BTreeMap<String, Option<bool>>;

fn read_store(path: &Path) -> Store {
	read_store_checked(path).unwrap_or_default()
}

/// Upstream `readTrustFile`: a missing file is empty; anything unreadable,
/// non-object, or holding a non-bool/null value is an error.
fn read_store_checked(path: &Path) -> io::Result<Store> {
	let text = match std::fs::read_to_string(path) {
		Ok(text) => text,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Store::new()),
		Err(error) => return Err(error),
	};
	let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
	let parsed: serde_json::Value =
		serde_json::from_str(text).map_err(|error| invalid_store(path, &error.to_string()))?;
	let Some(object) = parsed.as_object() else {
		return Err(invalid_store(path, "expected an object"));
	};
	let mut store = Store::new();
	for (key, value) in object {
		let decision = match value {
			serde_json::Value::Bool(decision) => Some(*decision),
			serde_json::Value::Null => None,
			other => {
				return Err(invalid_store(
					path,
					&format!("value for {key:?} must be true, false, or null (got {other})"),
				));
			}
		};
		store.insert(key.clone(), decision);
	}
	Ok(store)
}

fn invalid_store(path: &Path, reason: &str) -> io::Error {
	io::Error::new(
		io::ErrorKind::InvalidData,
		format!("invalid trust store {}: {reason}", path.display()),
	)
}

/// Upstream `writeTrustFile`: sorted keys, 2-space indent, trailing newline.
fn write_store(path: &Path, store: &Store) -> io::Result<()> {
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent)?;
	}
	let mut text = serde_json::to_string_pretty(store)
		.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
	text.push('\n');
	std::fs::write(path, text)
}

fn set_decision(store_path: &Path, cwd: &Path, decision: bool) -> io::Result<()> {
	let mut store = read_store_checked(store_path)?;
	store.insert(
		normalize_path(cwd).to_string_lossy().into_owned(),
		Some(decision),
	);
	write_store(store_path, &store)
}

/// Nearest recorded bool decision for `cwd`, walking up to the root —
/// upstream `findNearestTrustEntry`. `null` entries are skipped.
fn nearest_decision(store: &Store, cwd: &Path) -> Option<bool> {
	nearest_entry(store, cwd).map(|(_, decision)| decision)
}

/// [`nearest_decision`] with the path the decision was recorded under
/// (upstream `getEntry`'s `ProjectTrustStoreEntry.path`).
fn nearest_entry(store: &Store, cwd: &Path) -> Option<(PathBuf, bool)> {
	let mut current = normalize_path(cwd);
	loop {
		if let Some(Some(decision)) = store.get(current.to_string_lossy().as_ref()) {
			return Some((current, *decision));
		}
		if !current.pop() {
			return None;
		}
	}
}

/// Upstream `normalizeCwd` (`canonicalizePath(resolvePath(cwd))`): make
/// absolute, fold `.`/`..` lexically, then resolve symlinks when possible so
/// the stored key is stable across machines and temp-dir aliases.
fn normalize_path(path: &Path) -> PathBuf {
	let absolute = if path.is_absolute() {
		path.to_path_buf()
	} else {
		std::env::current_dir().unwrap_or_default().join(path)
	};
	let mut cleaned = PathBuf::new();
	for component in absolute.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				cleaned.pop();
			}
			other => cleaned.push(other),
		}
	}
	std::fs::canonicalize(&cleaned).unwrap_or(cleaned)
}

/// Ask on the terminal whether to trust the project's MCP config — the
/// pi-rs-shaped cut of upstream's `selectProjectTrustOption`. Raw mode is
/// restored on every path; Ctrl+C answers with an `Interrupted` error so the
/// caller exits instead of silently continuing untrusted.
pub fn prompt_trust(project_config: &Path) -> io::Result<bool> {
	struct RawMode;
	impl RawMode {
		fn enable() -> io::Result<Self> {
			terminal::enable_raw_mode()?;
			Ok(Self)
		}
	}
	impl Drop for RawMode {
		fn drop(&mut self) {
			let _ = terminal::disable_raw_mode();
		}
	}

	let _guard = RawMode::enable()?;
	let mut stdout = io::stdout();
	write!(
		stdout,
		"Trust project MCP config ({})? [y/N] ",
		project_config.display()
	)?;
	stdout.flush()?;
	let answer = loop {
		match crossterm::event::read()? {
			Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
				KeyCode::Char('y' | 'Y') => break true,
				KeyCode::Char('n' | 'N') | KeyCode::Enter | KeyCode::Esc => break false,
				KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
					return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
				}
				_ => {}
			},
			_ => {}
		}
	};
	writeln!(stdout, "{}", if answer { "y" } else { "n" })?;
	stdout.flush()?;
	Ok(answer)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	/// A throwaway store path and project directory.
	struct Fixture {
		dir: PathBuf,
		store: PathBuf,
	}

	impl Fixture {
		fn new(tag: &str) -> Self {
			let dir = std::env::temp_dir().join(format!(
				"pi-rs-trust-{tag}-{}-{}",
				std::process::id(),
				pi_ai::types::now_ms()
			));
			std::fs::create_dir_all(&dir).unwrap();
			let store = dir.join("trust.json");
			let _ = std::fs::remove_file(&store);
			Self { dir, store }
		}

		fn sub(&self, name: &str) -> PathBuf {
			let path = self.dir.join(name);
			std::fs::create_dir_all(&path).unwrap();
			path
		}
	}

	impl Drop for Fixture {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.dir);
		}
	}

	#[test]
	fn trust_roundtrips_through_the_store() {
		let fixture = Fixture::new("roundtrip");
		let project = fixture.sub("project");
		assert!(!is_trusted_at(&fixture.store, &project));
		trust_at(&fixture.store, &project).unwrap();
		assert!(is_trusted_at(&fixture.store, &project));
		// Re-trusting is idempotent.
		trust_at(&fixture.store, &project).unwrap();
		assert!(is_trusted_at(&fixture.store, &project));
	}

	#[test]
	fn set_and_clear_write_explicit_decisions_and_saved_entry_reports_the_path() {
		let fixture = Fixture::new("set-clear");
		let project = fixture.sub("project");
		// Untrusted is a stored false, not an absence.
		set_decision(&fixture.store, &project, false).unwrap();
		assert!(!is_trusted_at(&fixture.store, &project));
		let (path, decision) = saved_entry_at(&fixture.store, &project).expect("entry recorded");
		assert_eq!(path, super::normalize_path(&project));
		assert!(!decision);
		// Flipping back to trusted.
		set_decision(&fixture.store, &project, true).unwrap();
		let (_, decision) = saved_entry_at(&fixture.store, &project).unwrap();
		assert!(decision);
		// Clear leaves a null entry: no saved decision and no ancestor here,
		// so untrusted (upstream `set(cwd, null)`).
		let mut store = read_store_checked(&fixture.store).unwrap();
		store.insert(
			normalize_path(&project).to_string_lossy().into_owned(),
			None,
		);
		write_store(&fixture.store, &store).unwrap();
		assert_eq!(saved_entry_at(&fixture.store, &project), None);
		assert!(!is_trusted_at(&fixture.store, &project));
		let text = std::fs::read_to_string(&fixture.store).unwrap();
		assert!(
			text.contains("null"),
			"cleared entry stays in the store: {text}"
		);
	}

	#[test]
	fn an_ancestor_decision_covers_descendants() {
		let fixture = Fixture::new("ancestor");
		let child = fixture.sub("a").join("b");
		std::fs::create_dir_all(&child).unwrap();
		trust_at(&fixture.store, &fixture.dir).unwrap();
		assert!(is_trusted_at(&fixture.store, &child));
	}

	#[test]
	fn a_null_entry_clears_so_the_nearest_ancestor_decides() {
		let fixture = Fixture::new("null-clears");
		let child = fixture.sub("child");
		// Parent trusted, child explicitly cleared: the walk skips `null`
		// entries (upstream `findNearestTrustEntry` only matches bools), so
		// the nearest recorded ancestor decides.
		std::fs::write(
			&fixture.store,
			json!({
				normalize_path(&fixture.dir).to_string_lossy(): true,
				normalize_path(&child).to_string_lossy(): null,
			})
			.to_string(),
		)
		.unwrap();
		assert!(is_trusted_at(&fixture.store, &child));
		assert!(is_trusted_at(&fixture.store, &fixture.dir));
		// A direct bool still wins over the ancestor.
		std::fs::write(
			&fixture.store,
			json!({
				normalize_path(&fixture.dir).to_string_lossy(): true,
				normalize_path(&child).to_string_lossy(): false,
			})
			.to_string(),
		)
		.unwrap();
		assert!(!is_trusted_at(&fixture.store, &child));
	}

	#[test]
	fn the_store_is_sorted_json_with_a_trailing_newline() {
		let fixture = Fixture::new("shape");
		let b = fixture.sub("b");
		let a = fixture.sub("a");
		trust_at(&fixture.store, &b).unwrap();
		trust_at(&fixture.store, &a).unwrap();
		let text = std::fs::read_to_string(&fixture.store).unwrap();
		assert!(text.ends_with('\n'), "{text:?}");
		let key_a = normalize_path(&a).to_string_lossy().into_owned();
		let key_b = normalize_path(&b).to_string_lossy().into_owned();
		let (first, second) = if key_a < key_b {
			(key_a, key_b)
		} else {
			(key_b, key_a)
		};
		let first_pos = text.find(&first).expect("first key present");
		let second_pos = text.find(&second).expect("second key present");
		assert!(first_pos < second_pos, "keys are sorted: {text:?}");
		// Values round-trip as booleans.
		let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
		assert_eq!(parsed[first], json!(true));
		assert_eq!(parsed[second], json!(true));
	}

	#[test]
	fn corrupt_stores_are_untrusted_and_refuse_writes() {
		let fixture = Fixture::new("corrupt");
		std::fs::write(&fixture.store, "{not json").unwrap();
		let project = fixture.sub("p");
		assert!(!is_trusted_at(&fixture.store, &project));
		let error = trust_at(&fixture.store, &project).unwrap_err();
		assert_eq!(error.kind(), io::ErrorKind::InvalidData);

		// A well-formed object with a bad value type is rejected too.
		std::fs::write(
			&fixture.store,
			format!("{{\"{}\": \"yes\"}}", normalize_path(&project).display()),
		)
		.unwrap();
		assert!(!is_trusted_at(&fixture.store, &project));
	}

	#[test]
	fn normalization_folds_dot_segments_and_matches_canonical_paths() {
		let fixture = Fixture::new("normalize");
		let project = fixture.sub("p");
		let dotted = project.join("..").join("p");
		trust_at(&fixture.store, &project).unwrap();
		assert!(
			is_trusted_at(&fixture.store, &dotted),
			"{:?} must match {:?}",
			dotted,
			project
		);
	}
}
