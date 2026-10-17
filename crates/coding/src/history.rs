//! Prompt history for the TUI: Up/Down recall prompts from earlier runs.
//!
//! One JSON string per line, newest last, scoped to the working directory so
//! a prompt typed in one project does not show up in another:
//! `$PI_RS_HOME/history/<cwd slug>`. The newest `MAX_ENTRIES` are kept; older
//! lines are dropped when the file is loaded.
//!
//! History is a convenience, never a hard dependency: unreadable or malformed
//! files only mean an empty history, and append failures are ignored.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::util;

pub const MAX_ENTRIES: usize = 200;

pub fn path(cwd: &Path) -> PathBuf {
    util::home_dir().join("history").join(util::cwd_slug(cwd))
}

/// Entries for the current working directory, oldest first.
pub fn load() -> Vec<String> {
    load_from(&path(&current_dir()))
}

pub fn load_from(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries = parse(&text);
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
        let _ = rewrite(path, &entries);
    }
    entries
}

/// Remember a submitted prompt. Best effort: the TUI must not fail over it.
pub fn append(text: &str) {
    let _ = append_to(&path(&current_dir()), text);
}

pub fn append_to(path: &Path, text: &str) -> io::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let existing = fs::read_to_string(path).unwrap_or_default();
    // Re-submitting a recalled prompt is common; keep one entry.
    let last = existing
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<String>(line).ok());
    if last.as_deref() == Some(text) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(text).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        // A crash left a torn line; terminate it so it cannot swallow this one.
        writeln!(file)?;
    }
    writeln!(file, "{line}")
}

/// Skipped lines (a typo or a crash mid-write) never poison the rest.
fn parse(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<String>(line) else {
            continue;
        };
        if entry.trim().is_empty() || entries.last() == Some(&entry) {
            continue;
        }
        entries.push(entry);
    }
    entries
}

fn rewrite(path: &Path, entries: &[String]) -> io::Result<()> {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&serde_json::to_string(entry).map_err(io::Error::other)?);
        text.push('\n');
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    #[test]
    fn missing_file_is_an_empty_history() {
        let dir = temp_dir("history-missing");
        assert!(load_from(&dir.join("nope")).is_empty());
    }

    #[test]
    fn prompts_roundtrip_and_keep_their_newlines() {
        let file = temp_dir("history-roundtrip").join("history");
        append_to(&file, "first").unwrap();
        append_to(&file, "second\nline").unwrap();
        assert_eq!(load_from(&file), ["first", "second\nline"]);
    }

    #[test]
    fn consecutive_duplicates_and_blank_prompts_are_dropped() {
        let file = temp_dir("history-dupes").join("history");
        for text in ["same", "same", "   ", "other"] {
            append_to(&file, text).unwrap();
        }
        assert_eq!(load_from(&file), ["same", "other"]);
        assert_eq!(fs::read_to_string(&file).unwrap().lines().count(), 2);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = temp_dir("history-malformed");
        let file = dir.join("history");
        fs::write(&file, "\"one\"\nnot json\n\"two\"\n").unwrap();
        assert_eq!(load_from(&file), ["one", "two"]);
    }

    #[test]
    fn a_torn_last_line_does_not_swallow_the_next_prompt() {
        let dir = temp_dir("history-torn");
        let file = dir.join("history");
        fs::write(&file, "\"one\"\n\"tor").unwrap();
        append_to(&file, "two").unwrap();
        assert_eq!(load_from(&file), ["one", "two"]);
    }

    #[test]
    fn old_entries_are_dropped_at_load_and_the_file_shrinks() {
        let dir = temp_dir("history-cap");
        let file = dir.join("history");
        for i in 0..MAX_ENTRIES + 5 {
            append_to(&file, &format!("p{i}")).unwrap();
        }
        let entries = load_from(&file);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().unwrap(), "p5");
        assert_eq!(entries.last().unwrap(), &format!("p{}", MAX_ENTRIES + 4));

        let lines = fs::read_to_string(&file).unwrap().lines().count();
        assert_eq!(lines, MAX_ENTRIES);
    }

    #[test]
    fn history_is_scoped_per_directory() {
        assert_ne!(
            path(Path::new("/work/one")),
            path(Path::new("/work/two")),
            "different projects must not share prompt history"
        );
    }
}
