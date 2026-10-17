use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Data directory: `$PI_RS_HOME` or `~/.pi-rs`.
pub fn home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_RS_HOME") {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".pi-rs"),
        None => PathBuf::from(".pi-rs"),
    }
}

/// Sessions and prompt history live under a slug of the working directory,
/// mirroring Pi: `/Users/andi/ws/oc/pi-rs` → `Users-andi-ws-oc-pi-rs`.
pub fn cwd_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}

pub fn sessions_dir(cwd: &Path) -> PathBuf {
    home_dir().join("sessions").join(cwd_slug(cwd))
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Short unique id for entries (hex nanoseconds + counter).
pub fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:012x}{counter:02x}")
}

/// UTC timestamp without pulling in a date library: `2026-10-17T14:03:22Z`.
pub fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_from_unix(secs)
}

pub fn iso_from_unix(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Filesystem-safe timestamp for session file names.
pub fn file_timestamp() -> String {
    iso_now().replace(':', "-")
}

/// One-line summary of tool arguments, for display in a terminal.
pub fn summarize_args(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return truncate_chars(raw, 80);
    };
    const KEYS: [&str; 6] = ["command", "file_path", "path", "pattern", "query", "url"];
    if let Some(map) = value.as_object() {
        for key in KEYS {
            if let Some(s) = map.get(key).and_then(|v| v.as_str()) {
                return truncate_chars(&s.split_whitespace().collect::<Vec<_>>().join(" "), 80);
            }
        }
    }
    truncate_chars(&raw.split_whitespace().collect::<Vec<_>>().join(" "), 80)
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Keep at most `head` + `tail` lines, with a marker in between.
pub fn truncate_lines(text: &str, head: usize, tail: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head + tail {
        return text.to_string();
    }
    let mut out = String::new();
    for line in &lines[..head] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "… [{} lines omitted] …\n",
        lines.len() - head - tail
    ));
    for (i, line) in lines[lines.len() - tail..].iter().enumerate() {
        out.push_str(line);
        if i + 1 < tail {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamps() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        // 2026-10-17T14:03:22Z
        assert_eq!(iso_from_unix(1_792_245_802), "2026-10-17T14:03:22Z");
    }

    #[test]
    fn line_truncation() {
        let text = (1..=10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            truncate_lines(&text, 3, 2),
            "1\n2\n3\n… [5 lines omitted] …\n9\n10"
        );
        assert_eq!(truncate_lines("a\nb", 3, 2), "a\nb");
    }

    #[test]
    fn arg_summaries() {
        assert_eq!(
            summarize_args(r#"{"command":"cargo build --release"}"#),
            "cargo build --release"
        );
        assert_eq!(
            summarize_args(r#"{"file_path":"src/main.rs","offset":1}"#),
            "src/main.rs"
        );
        assert_eq!(summarize_args("not json"), "not json");
        assert!(summarize_args(r#"{"command":"x"}"#).len() <= 81);
    }
}
