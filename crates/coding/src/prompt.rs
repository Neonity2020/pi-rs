//! System prompt assembly.
//!
//! Same discipline as Pi: a short base prompt, plus `SYSTEM.md` (which
//! replaces the base entirely if present) plus `AGENTS.md` files merged
//! root-down, deepest last, plus a bounded long-term memory section.
//! No hidden context.

use std::path::{Path, PathBuf};

use crate::{memory, util};

/// Build the system prompt for a working directory.
pub fn build(cwd: &Path) -> String {
    build_with(cwd, &util::home_dir())
}

fn build_with(cwd: &Path, home: &Path) -> String {
    let mut prompt = match find_system_md(cwd, home) {
        Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
        None => default_prompt(cwd, home),
    };

    for (path, content) in agents_files(cwd, home) {
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        prompt.push_str(&format!(
            "\n\n# Instructions from {}\n\n{content}",
            path.display()
        ));
    }

    // Memory goes last: it is context, not instructions, and keeping it at the
    // end leaves the base + AGENTS prefix stable for provider prompt caching.
    prompt.push_str("\n\n");
    prompt.push_str(&memory::render_section(home, cwd));
    prompt
}

fn find_system_md(cwd: &Path, home: &Path) -> Option<PathBuf> {
    for dir in [cwd, home] {
        let candidate = dir.join("SYSTEM.md");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Global `AGENTS.md` first, then every `AGENTS.md` from the filesystem root
/// down to `cwd` (deepest last, so project-specific wins).
fn agents_files(cwd: &Path, home: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let mut seen = Vec::new();

    let global = home.join("AGENTS.md");
    if global.is_file() {
        if let Ok(content) = std::fs::read_to_string(&global) {
            seen.push(global.clone());
            files.push((global, content));
        }
    }

    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    for dir in chain {
        let candidate = dir.join("AGENTS.md");
        if candidate.is_file() && !seen.contains(&candidate) {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                files.push((candidate, content));
            }
        }
    }
    files
}

fn default_prompt(cwd: &Path, home: &Path) -> String {
    format!(
        "You are pi-rs, a coding agent working in the user's terminal.

You have four tools:
- read: read a file's contents (line-numbered).
- write: create or overwrite a file.
- edit: replace an exact string in a file.
- bash: run a shell command — use it for searching, building, testing, git, anything the other three don't cover.

Rules:
- Read a file before editing it. Prefer edit over write for existing files.
- Search with bash (rg, grep, find). Keep commands short and targeted.
- Verify your work: run the tests or the command you changed. Never claim something works without running it.
- For long tasks, write the plan to PLAN.md and track to-dos in TODO.md. Files survive restarts; hidden state does not.
- Be concise. No filler, no flattery, no restating the request.

Environment:
- cwd: {}
- platform: {} {}
- date: {}
- global instructions: {}/AGENTS.md (loaded before root-to-cwd AGENTS.md)",
        cwd.display(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        util::iso_now(),
        home.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-rs-prompt-{}-{name}", util::short_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_prompt_mentions_environment() {
        let cwd = Path::new("/tmp/project");
        let home = Path::new("/tmp/pi-rs-home");
        let prompt = default_prompt(cwd, home);
        assert!(prompt.contains("cwd: /tmp/project"));
        assert!(prompt.contains("global instructions: /tmp/pi-rs-home/AGENTS.md"));
        assert!(prompt.contains("loaded before root-to-cwd AGENTS.md"));
        assert!(prompt.contains("read"));
        assert!(prompt.len() < 2500, "keep the base prompt small");
    }

    #[test]
    fn agents_files_merge_root_down() {
        let home = temp_dir("home");
        let root = temp_dir("root");
        std::fs::write(home.join("AGENTS.md"), "global").unwrap();
        std::fs::write(root.join("AGENTS.md"), "root").unwrap();
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested").unwrap();

        let files = agents_files(&nested, &home);
        let names: Vec<String> = files
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 3);
        let contents: Vec<&str> = files.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(contents, vec!["global", "root", "nested"]);
    }

    #[test]
    fn system_md_replaces_the_base_prompt() {
        let home = temp_dir("home2");
        let cwd = temp_dir("proj2");
        std::fs::write(cwd.join("SYSTEM.md"), "CUSTOM PROMPT").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "extra rules").unwrap();
        let prompt = build_with(&cwd, &home);
        assert!(prompt.starts_with("CUSTOM PROMPT"));
        assert!(prompt.contains("extra rules"));
        assert!(prompt.contains("# Long-term memory"));
        assert!(!prompt.contains("You are pi-rs, a coding agent"));
    }

    #[test]
    fn memory_follows_agents_instructions() {
        let home = temp_dir("home3");
        let cwd = temp_dir("proj3");
        std::fs::write(cwd.join("AGENTS.md"), "project rules").unwrap();
        std::fs::write(home.join("MEMORY.md"), "- (2026-10-17) global note\n").unwrap();
        std::fs::create_dir_all(home.join("memory")).unwrap();
        std::fs::write(
            memory::project_path(&home, &cwd),
            "- (2026-10-16) project note\n",
        )
        .unwrap();

        let prompt = build_with(&cwd, &home);
        let agents = prompt.find("project rules").unwrap();
        let memory = prompt.find("# Long-term memory").unwrap();
        assert!(agents < memory, "memory should come after AGENTS.md");
        assert!(prompt.contains("global note"));
        assert!(prompt.contains("project note"));
        assert!(prompt.contains(&home.join("MEMORY.md").display().to_string()));
    }
}
