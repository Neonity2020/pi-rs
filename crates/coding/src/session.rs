//! Sessions: append-only JSONL, one file per conversation.
//!
//! Every entry carries `id` + `parent`, so a file is a tree, not a log.
//! Nothing in pi-rs navigates the tree yet (no branching UI), but the format
//! is the one we can grow into without rewriting history.
//!
//! Compaction entries summarize the prefix of the branch. They are written in
//! place (the old messages stay in the file), and loading replays the latest
//! one: a synthetic summary message followed by the kept tail.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use agent::summary_message;
use anyhow::{bail, Context, Result};
use llm::Message;
use serde::{Deserialize, Serialize};

use crate::util;

#[derive(Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    v: u32,
    id: String,
    cwd: String,
    model: String,
    created: String,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    id: String,
    parent: Option<String>,
    ts: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction: Option<CompactionEntry>,
}

#[derive(Serialize, Deserialize)]
struct CompactionEntry {
    summary: String,
    first_kept_id: String,
    tokens_before: u64,
    tokens_after: u64,
}

/// An open session. `path` is the JSONL file; `last` is the tip of the
/// active branch, used as `parent` for the next entry.
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    last: Option<String>,
    /// Summary from the latest compaction, replayed by the agent so it can
    /// update instead of re-summarizing the whole prefix.
    summary: Option<String>,
    /// Entry id for each message in the current context. The synthetic
    /// summary message has no entry of its own, hence `None`.
    message_ids: Vec<Option<String>>,
}

impl Session {
    /// Create a fresh session for `cwd` inside `dir`.
    pub fn create_in(dir: &Path, cwd: &Path, model: &str) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        // The timestamp has second resolution, so the full id is what keeps
        // two sessions created in the same second apart. Never truncate an
        // existing file: `create_new` turns a collision into a retry.
        let (mut file, path, id) = {
            let mut attempts = 0;
            loop {
                let id = util::short_id();
                let path = dir.join(format!("{}-{id}.jsonl", util::file_timestamp()));
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(file) => break (file, path, id),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 =>
                    {
                        attempts += 1;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("create {}", path.display()))
                    }
                }
            }
        };
        let header = Header {
            kind: "session".into(),
            v: 2,
            id: id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: model.to_string(),
            created: util::iso_now(),
        };
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        Ok(Self {
            id,
            path,
            last: None,
            summary: None,
            message_ids: Vec::new(),
        })
    }

    pub fn create(cwd: &Path, model: &str) -> Result<Self> {
        Self::create_in(&util::sessions_dir(cwd), cwd, model)
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    fn append_entry(
        &mut self,
        kind: &str,
        message: Option<Message>,
        compaction: Option<CompactionEntry>,
    ) -> Result<String> {
        let id = util::short_id();
        let entry = Entry {
            id: id.clone(),
            parent: self.last.clone(),
            ts: util::iso_now(),
            kind: kind.to_string(),
            message,
            compaction,
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)
            .with_context(|| format!("append {}", self.path.display()))?;
        self.last = Some(id.clone());
        Ok(id)
    }

    pub fn append_message(&mut self, message: &Message) -> Result<()> {
        let id = self.append_entry("message", Some(message.clone()), None)?;
        self.message_ids.push(Some(id));
        Ok(())
    }

    /// Record a compaction boundary. `first_kept_index` indexes the current
    /// message list (including a leading synthetic summary at index 0).
    pub fn append_compaction(
        &mut self,
        summary: &str,
        first_kept_index: usize,
        tokens_before: u64,
        tokens_after: u64,
    ) -> Result<()> {
        let first_kept_id = self
            .message_ids
            .get(first_kept_index)
            .and_then(Clone::clone)
            .or_else(|| {
                self.message_ids
                    .iter()
                    .skip(first_kept_index)
                    .flatten()
                    .next()
                    .cloned()
            })
            .context("compaction boundary has no session entry")?;

        let kept = if first_kept_index >= self.message_ids.len() {
            Vec::new()
        } else {
            self.message_ids[first_kept_index..].to_vec()
        };
        let entry = CompactionEntry {
            summary: summary.to_string(),
            first_kept_id,
            tokens_before,
            tokens_after,
        };
        self.append_entry("compaction", None, Some(entry))?;
        self.summary = Some(summary.to_string());
        self.message_ids = std::iter::once(None).chain(kept).collect();
        Ok(())
    }

    /// Most recent session in `dir`, with its active branch reconstructed.
    pub fn load_latest_in(dir: &Path) -> Result<Option<(Self, Vec<Message>)>> {
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
        for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(_, best)| modified > *best) {
                newest = Some((path, modified));
            }
        }
        let Some((path, _)) = newest else {
            return Ok(None);
        };
        Self::load_file(&path).map(Some)
    }

    pub fn load_latest(cwd: &Path) -> Result<Option<(Self, Vec<Message>)>> {
        Self::load_latest_in(&util::sessions_dir(cwd))
    }

    fn load_file(path: &Path) -> Result<(Self, Vec<Message>)> {
        // Read bytes: a crash can tear a UTF-8 character in the final record.
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let lines: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
        let nonempty = |line: &&[u8]| !line.iter().all(u8::is_ascii_whitespace);
        let first = lines
            .iter()
            .position(nonempty)
            .context("empty session file")?;
        let last_line = lines.iter().rposition(nonempty).unwrap();
        let header: Header =
            serde_json::from_slice(lines[first]).context("parse session header")?;
        if header.kind != "session" {
            bail!("{} is not a session file", path.display());
        }
        if header.v > 2 {
            bail!(
                "{} uses session format v{}; this pi-rs understands v2",
                path.display(),
                header.v
            );
        }

        let mut entries: Vec<Entry> = Vec::new();
        let mut offset = 0;
        let mut torn_at = None;
        for (number, line) in lines.iter().enumerate() {
            let start = offset;
            offset += line.len();
            if number <= first || !nonempty(line) {
                continue;
            }
            match serde_json::from_slice::<Entry>(line) {
                Ok(entry) => entries.push(entry),
                Err(_) if number == last_line => torn_at = Some(start),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("corrupt session {} at line {}", path.display(), number + 1)
                    })
                }
            }
        }

        let mut by_id = HashMap::new();
        for entry in &entries {
            if by_id.insert(entry.id.as_str(), entry).is_some() {
                bail!(
                    "duplicate session entry id {} in {}",
                    entry.id,
                    path.display()
                );
            }
        }
        // Validate every branch before repairing anything. A visited set also
        // bounds traversal when an edited/damaged file contains a parent cycle.
        let mut checked = HashSet::new();
        for entry in &entries {
            let mut visiting = HashSet::new();
            let mut cursor = Some(entry.id.as_str());
            while let Some(id) = cursor {
                if checked.contains(id) {
                    break;
                }
                if !visiting.insert(id) {
                    bail!("session parent cycle at {id} in {}", path.display());
                }
                let entry = by_id.get(id).with_context(|| {
                    format!("missing session parent {id} in {}", path.display())
                })?;
                cursor = entry.parent.as_deref();
            }
            checked.extend(visiting);
        }

        repair_tail(path, &bytes, torn_at)?;

        // Walk the active branch: from the last entry back to the root.
        let mut branch: Vec<&Entry> = Vec::new();
        let mut cursor = entries.last().map(|e| e.id.as_str());
        while let Some(id) = cursor {
            let entry = by_id[id];
            branch.push(entry);
            cursor = entry.parent.as_deref();
        }
        branch.reverse();

        let mut messages: Vec<Message> = Vec::new();
        let mut message_ids: Vec<Option<String>> = Vec::new();
        let mut summary = None;

        let latest = branch
            .iter()
            .rposition(|entry| entry.kind == "compaction" && entry.compaction.is_some());
        let kept_from = latest.and_then(|index| {
            let record = branch[index].compaction.as_ref()?;
            branch
                .iter()
                .position(|entry| entry.id == record.first_kept_id)
                .filter(|&start| start < index)
                .map(|start| (start, record))
        });

        if let Some((start, record)) = kept_from {
            summary = Some(record.summary.clone());
            messages.push(summary_message(&record.summary));
            message_ids.push(None);
            for entry in &branch[start..] {
                if let Some(message) = &entry.message {
                    messages.push(message.clone());
                    message_ids.push(Some(entry.id.clone()));
                }
            }
        } else {
            // No compaction, or a broken `first_kept_id`: fall back to the
            // complete branch rather than silently dropping history.
            for entry in &branch {
                if let Some(message) = &entry.message {
                    messages.push(message.clone());
                    message_ids.push(Some(entry.id.clone()));
                }
            }
        }

        let last = entries.last().map(|e| e.id.clone());
        Ok((
            Self {
                id: header.id,
                path: path.to_path_buf(),
                last,
                summary,
                message_ids,
            },
            messages,
        ))
    }
}

/// The only truncation allowed for append-only sessions is a damaged tail.
/// Keep the original bytes before removing it; retained records are untouched.
fn repair_tail(path: &Path, bytes: &[u8], torn_at: Option<usize>) -> Result<()> {
    let kept = &bytes[..torn_at.unwrap_or(bytes.len())];
    if torn_at.is_none() && kept.ends_with(b"\n") {
        return Ok(());
    }
    if torn_at.is_some() {
        let backup = path.with_extension(format!("jsonl.{}.bak", util::short_id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&backup)
            .with_context(|| format!("create session backup {}", backup.display()))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .with_context(|| format!("save session backup {}", backup.display()))?;
        eprintln!(
            "warning: repairing damaged session tail; original saved to {}",
            backup.display()
        );
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("repair session {}", path.display()))?;
    if torn_at.is_some() {
        file.set_len(kept.len() as u64)?;
    }
    if !kept.ends_with(b"\n") {
        file.write_all(b"\n")?;
    }
    file.sync_all()
        .with_context(|| format!("repair session {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::ToolCall;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-rs-test-{}-{name}", util::short_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip() {
        let dir = temp_dir("roundtrip");
        let cwd = Path::new("/tmp/project");
        let mut session = Session::create_in(&dir, cwd, "deepseek-flash").unwrap();
        session.append_message(&Message::user("hi")).unwrap();
        session
            .append_message(&Message::assistant(
                Some("calling".into()),
                Some("thinking".into()),
                vec![ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                }],
            ))
            .unwrap();
        session
            .append_message(&Message::tool("c1", "file.txt"))
            .unwrap();

        let (loaded, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, llm::Role::User);
        assert_eq!(messages[1].reasoning.as_deref(), Some("thinking"));
        assert_eq!(messages[1].tool_calls[0].id, "c1");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[2].text(), "file.txt");
        assert_eq!(loaded.summary(), None);
    }

    #[test]
    fn sessions_created_in_the_same_second_keep_separate_files() {
        let dir = temp_dir("same-second");
        let cwd = Path::new("/tmp/project");
        let mut first = Session::create_in(&dir, cwd, "m").unwrap();
        first.append_message(&Message::user("first")).unwrap();
        let first_path = first.path.clone();
        let original = fs::read(&first_path).unwrap();

        let mut second = Session::create_in(&dir, cwd, "m").unwrap();
        second.append_message(&Message::user("second")).unwrap();

        // The second-resolution timestamp is not enough on its own; the full
        // id must be in the file name so same-second sessions cannot collide.
        assert_ne!(first.path, second.path);
        assert!(first.path.to_string_lossy().contains(&first.id));
        assert!(second.path.to_string_lossy().contains(&second.id));
        // Creating a second session must never truncate the first.
        assert_eq!(fs::read(&first_path).unwrap(), original);
        let (_, messages) = Session::load_file(&first_path).unwrap();
        assert_eq!(messages[0].text(), "first");
        let (_, messages) = Session::load_file(&second.path).unwrap();
        assert_eq!(messages[0].text(), "second");
    }

    #[test]
    fn images_survive_a_session_roundtrip() {
        let dir = temp_dir("images");
        let cwd = Path::new("/tmp/project");
        let mut session = Session::create_in(&dir, cwd, "m").unwrap();
        session
            .append_message(&Message::user_with_images(
                "look",
                vec![llm::Image::new("image/png", "iVBORw0KGgo=")],
            ))
            .unwrap();

        let (_, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].images.len(), 1);
        assert_eq!(messages[0].images[0].media_type, "image/png");
        assert_eq!(messages[0].images[0].data, "iVBORw0KGgo=");
        assert_eq!(messages[0].text(), "look");
    }

    #[test]
    fn resume_continues_the_branch() {
        let dir = temp_dir("resume");
        let cwd = Path::new("/tmp/project");
        let mut session = Session::create_in(&dir, cwd, "m").unwrap();
        session.append_message(&Message::user("one")).unwrap();
        drop(session);

        let (mut session, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 1);
        session.append_message(&Message::user("two")).unwrap();

        let (_, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "two");
        // The second message must hang off the first, not off the root.
        let text = fs::read_to_string(&session.path).unwrap();
        let entries: Vec<Entry> = text
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(entries[1].parent.as_deref(), Some(entries[0].id.as_str()));
    }

    #[test]
    fn compaction_replays_summary_plus_kept_tail() {
        let dir = temp_dir("compaction");
        let cwd = Path::new("/tmp/project");
        let mut session = Session::create_in(&dir, cwd, "m").unwrap();
        session.append_message(&Message::user("old")).unwrap();
        session
            .append_message(&Message::assistant(Some("old answer".into()), None, vec![]))
            .unwrap();
        session.append_message(&Message::user("recent")).unwrap();
        session
            .append_compaction("## Goal\ncontinue", 2, 10_000, 2_000)
            .unwrap();

        let (loaded, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(agent::is_summary_message(&messages[0]));
        assert!(messages[0].text().contains("## Goal\ncontinue"));
        assert_eq!(messages[1].text(), "recent");
        assert_eq!(loaded.summary(), Some("## Goal\ncontinue"));
        assert_eq!(loaded.message_ids.len(), 2);
        assert_eq!(loaded.message_ids[0], None);
        assert!(loaded.message_ids[1].is_some());
    }

    #[test]
    fn a_second_compaction_only_keeps_the_latest_tail() {
        let dir = temp_dir("second-compaction");
        let cwd = Path::new("/tmp/project");
        let mut session = Session::create_in(&dir, cwd, "m").unwrap();
        session.append_message(&Message::user("one")).unwrap();
        session.append_message(&Message::user("two")).unwrap();
        session.append_message(&Message::user("three")).unwrap();
        session
            .append_compaction("first summary", 2, 100, 50)
            .unwrap();

        let (mut session, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(agent::is_summary_message(&messages[0]));
        assert_eq!(messages[1].text(), "three");

        session.append_message(&Message::user("four")).unwrap();
        session
            .append_compaction("second summary", 2, 200, 60)
            .unwrap();

        let (loaded, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(loaded.summary(), Some("second summary"));
        assert!(messages[0].text().contains("second summary"));
        assert_eq!(messages[1].text(), "four");
    }

    #[test]
    fn broken_compaction_boundary_falls_back_to_full_history() {
        let dir = temp_dir("broken-compaction");
        let cwd = Path::new("/tmp/project");
        let mut session = Session::create_in(&dir, cwd, "m").unwrap();
        session.append_message(&Message::user("one")).unwrap();
        session.append_message(&Message::user("two")).unwrap();
        // A broken id skips the synthetic summary and keeps everything.
        let entry = CompactionEntry {
            summary: "lost".into(),
            first_kept_id: "missing".into(),
            tokens_before: 10,
            tokens_after: 5,
        };
        session
            .append_entry("compaction", None, Some(entry))
            .unwrap();

        let (loaded, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "one");
        assert_eq!(messages[1].text(), "two");
        assert_eq!(loaded.summary(), None);
    }

    #[test]
    fn empty_dir_has_no_session() {
        let dir = temp_dir("empty");
        assert!(Session::load_latest_in(&dir).unwrap().is_none());
    }

    #[test]
    fn torn_tail_can_resume_append_and_resume_again() {
        for tail in [b"{\"id\":\"torn".as_slice(), b"{\"id\":\"\xff", b"broken\n"] {
            let dir = temp_dir("torn-tail");
            let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
            session.append_message(&Message::user("one")).unwrap();
            let path = session.path.clone();
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(tail)
                .unwrap();
            let original = fs::read(&path).unwrap();

            let (mut resumed, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
            assert_eq!(messages[0].text(), "one");
            resumed.append_message(&Message::user("two")).unwrap();
            let (mut resumed, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
            assert_eq!(
                messages.iter().map(Message::text).collect::<Vec<_>>(),
                ["one", "two"]
            );
            resumed.append_message(&Message::user("three")).unwrap();
            let (_, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
            assert_eq!(messages.len(), 3);
            let backups: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "bak"))
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(fs::read(&backups[0]).unwrap(), original);
            for line in fs::read_to_string(path).unwrap().lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
        }
    }

    #[test]
    fn valid_unterminated_message_is_preserved_before_appending() {
        let dir = temp_dir("no-newline");
        let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
        session.append_message(&Message::user("one")).unwrap();
        let text = fs::read_to_string(&session.path).unwrap();
        fs::write(&session.path, text.trim_end()).unwrap();
        let (mut resumed, _) = Session::load_latest_in(&dir).unwrap().unwrap();
        resumed.append_message(&Message::user("two")).unwrap();
        let (_, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn legacy_v1_session_can_resume_and_append() {
        let dir = temp_dir("legacy-v1");
        let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
        session.append_message(&Message::user("one")).unwrap();
        let text = fs::read_to_string(&session.path)
            .unwrap()
            .replacen("\"v\":2", "\"v\":1", 1);
        fs::write(&session.path, text).unwrap();
        let (mut resumed, _) = Session::load_latest_in(&dir).unwrap().unwrap();
        resumed.append_message(&Message::user("two")).unwrap();
        let (_, messages) = Session::load_latest_in(&dir).unwrap().unwrap();
        assert_eq!(
            messages.iter().map(Message::text).collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn interior_corruption_is_rejected_without_changing_the_file() {
        let dir = temp_dir("bad-middle");
        let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
        session.append_message(&Message::user("one")).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&session.path)
            .unwrap()
            .write_all(b"broken\n")
            .unwrap();
        session.append_message(&Message::user("two")).unwrap();
        let original = fs::read(&session.path).unwrap();
        assert!(Session::load_latest_in(&dir).is_err());
        assert_eq!(fs::read(&session.path).unwrap(), original);
    }

    #[test]
    fn invalid_parent_links_are_rejected_without_changing_the_file() {
        for defect in ["duplicate", "missing", "cycle"] {
            let dir = temp_dir(defect);
            let mut session = Session::create_in(&dir, Path::new("/tmp/project"), "m").unwrap();
            session.append_message(&Message::user("one")).unwrap();
            session.append_message(&Message::user("two")).unwrap();
            let text = fs::read_to_string(&session.path).unwrap();
            let mut lines: Vec<serde_json::Value> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            match defect {
                "duplicate" => lines[2]["id"] = lines[1]["id"].clone(),
                "missing" => lines[2]["parent"] = "absent".into(),
                "cycle" => lines[1]["parent"] = lines[2]["id"].clone(),
                _ => unreachable!(),
            }
            let damaged = lines.iter().map(|l| format!("{l}\n")).collect::<String>();
            fs::write(&session.path, &damaged).unwrap();
            assert!(Session::load_latest_in(&dir).is_err(), "accepted {defect}");
            assert_eq!(fs::read_to_string(&session.path).unwrap(), damaged);
        }
    }
}
