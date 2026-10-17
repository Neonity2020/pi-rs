//! Sessions: append-only JSONL, one file per conversation.
//!
//! Every entry carries `id` + `parent`, so a file is a tree, not a log.
//! Nothing in pi-rs navigates the tree yet (no branching UI), but the format
//! is the one we can grow into without rewriting history.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

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
    message: Message,
}

/// An open session. `path` is the JSONL file; `last` is the tip of the
/// active branch, used as `parent` for the next entry.
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    last: Option<String>,
}

impl Session {
    /// Create a fresh session for `cwd` inside `dir`.
    pub fn create_in(dir: &Path, cwd: &Path, model: &str) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let id = util::short_id();
        let file_name = format!(
            "{}-{}.jsonl",
            util::file_timestamp(),
            &id[..8.min(id.len())]
        );
        let path = dir.join(file_name);
        let header = Header {
            kind: "session".into(),
            v: 1,
            id: id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: model.to_string(),
            created: util::iso_now(),
        };
        let mut file =
            fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        Ok(Self {
            id,
            path,
            last: None,
        })
    }

    pub fn create(cwd: &Path, model: &str) -> Result<Self> {
        Self::create_in(&util::sessions_dir(cwd), cwd, model)
    }

    fn append(&mut self, message: Message) -> Result<()> {
        let id = util::short_id();
        let entry = Entry {
            id: id.clone(),
            parent: self.last.clone(),
            ts: util::iso_now(),
            kind: "message".into(),
            message,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        self.last = Some(id);
        Ok(())
    }

    pub fn append_message(&mut self, message: &Message) -> Result<()> {
        self.append(message.clone())
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
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());

        let header_line = lines.next().context("empty session file")?;
        let header: Header = serde_json::from_str(header_line).context("parse session header")?;
        if header.kind != "session" {
            bail!("{} is not a session file", path.display());
        }

        let mut entries: Vec<Entry> = Vec::new();
        for line in lines {
            match serde_json::from_str::<Entry>(line) {
                Ok(entry) => entries.push(entry),
                // A torn final line (crash mid-write) should not kill resume.
                Err(_) => continue,
            }
        }

        // Walk the active branch: from the last entry back to the root.
        let mut branch: Vec<&Entry> = Vec::new();
        let mut cursor = entries.last().map(|e| e.id.clone());
        while let Some(id) = cursor {
            let Some(entry) = entries.iter().find(|e| e.id == id) else {
                break;
            };
            branch.push(entry);
            cursor = entry.parent.clone();
        }
        branch.reverse();

        let messages = branch.into_iter().map(|e| e.message.clone()).collect();
        let last = entries.last().map(|e| e.id.clone());
        Ok((
            Self {
                id: header.id,
                path: path.to_path_buf(),
                last,
            },
            messages,
        ))
    }
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
    fn empty_dir_has_no_session() {
        let dir = temp_dir("empty");
        assert!(Session::load_latest_in(&dir).unwrap().is_none());
    }
}
