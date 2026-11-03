//! Changelog — port of `pi/packages/coding-agent/src/utils/changelog.ts`
//! (`parseChangelog` slice) plus `config.ts` `getChangelogPath`.
//!
//! `CHANGELOG.md` is keepachangelog-style: `## [x.y.z] - date` sections whose
//! bodies run to the next `## ` header or EOF. The copy is embedded at build
//! time (`include_str!`) so released binaries carry it; a `CHANGELOG.md` next
//! to the executable wins when present, mirroring upstream's package-dir
//! lookup. The link-normalization half of the upstream module is not ported —
//! pi-rs entries carry no repo-relative links to rewrite.

/// One `## [x.y.z] - date` section — upstream `ChangelogEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
	pub major: u32,
	pub minor: u32,
	pub patch: u32,
	/// The header line plus the body up to the next `## `, trimmed.
	pub content: String,
}

/// Parse keepachangelog-style version sections out of a changelog —
/// upstream `parseChangelog`: scan `## ` lines, collect content until the
/// next `## ` (or EOF), skip sections whose version does not parse.
pub fn parse_changelog(content: &str) -> Vec<ChangelogEntry> {
	let mut entries = Vec::new();
	let mut current_version: Option<(u32, u32, u32)> = None;
	let mut current_lines: Vec<&str> = Vec::new();

	for line in content.split('\n') {
		if line.starts_with("## ") {
			if let Some((major, minor, patch)) = current_version
				&& !current_lines.is_empty()
			{
				entries.push(ChangelogEntry {
					major,
					minor,
					patch,
					content: current_lines.join("\n").trim().to_string(),
				});
			}
			current_version = parse_version_header(line);
			// A `## ` line whose version does not parse resets the section
			// (upstream drops it); a parsed one starts a new entry body.
			current_lines = if current_version.is_some() {
				vec![line]
			} else {
				Vec::new()
			};
		} else if current_version.is_some() {
			current_lines.push(line);
		}
	}
	if let Some((major, minor, patch)) = current_version
		&& !current_lines.is_empty()
	{
		entries.push(ChangelogEntry {
			major,
			minor,
			patch,
			content: current_lines.join("\n").trim().to_string(),
		});
	}
	entries
}

/// `## [1.2.3] - 2026-01-01` → `(1, 2, 3)` (upstream
/// `/##\s+\[?(\d+)\.(\d+)\.(\d+)\]?/`).
fn parse_version_header(line: &str) -> Option<(u32, u32, u32)> {
	let rest = line.strip_prefix("##")?;
	let rest = rest.trim_start();
	let rest = rest.strip_prefix('[').unwrap_or(rest);
	let (version, _tail) = rest.split_once(']').unwrap_or((rest, ""));
	let mut parts = version.split('.');
	let major = parts.next()?.parse().ok()?;
	let minor = parts.next()?.parse().ok()?;
	let patch = parts.next()?.parse().ok()?;
	Some((major, minor, patch))
}

/// The embedded pi-rs changelog (repo-root `CHANGELOG.md` at build time).
const EMBEDDED_CHANGELOG: &str = include_str!("../../../../CHANGELOG.md");

/// Path of a `CHANGELOG.md` sitting next to the running executable
/// (upstream `getChangelogPath` resolves the package dir; a shipped binary's
/// package dir is its own directory). `None` when no file is there.
fn exe_side_changelog() -> Option<std::path::PathBuf> {
	let exe = std::env::current_exe().ok()?;
	Some(exe.parent()?.join("CHANGELOG.md"))
}

/// The changelog text `/changelog` renders: a `CHANGELOG.md` next to the
/// executable when one exists, else the embedded copy.
pub fn changelog_text() -> String {
	if let Some(path) = exe_side_changelog()
		&& let Ok(text) = std::fs::read_to_string(&path)
	{
		return text;
	}
	EMBEDDED_CHANGELOG.to_string()
}

/// The `/changelog` markdown: every entry newest-first (upstream reverses the
/// parse order), or the upstream no-entries message.
pub fn changelog_markdown() -> String {
	changelog_markdown_from(&changelog_text())
}

/// [`changelog_markdown`] over an explicit text.
pub fn changelog_markdown_from(text: &str) -> String {
	let mut entries = parse_changelog(text);
	if entries.is_empty() {
		return "No changelog entries found.".to_string();
	}
	entries.reverse();
	entries
		.iter()
		.map(|entry| entry.content.clone())
		.collect::<Vec<_>>()
		.join("\n\n")
}

#[cfg(test)]
mod tests {
	use super::*;

	const SAMPLE: &str = "\
# Changelog\n\
\n\
## [1.2.3] - 2026-11-01\n\
\n\
### Added\n\
\n\
- first feature\n\
- second feature\n\
\n\
## [1.2.2] - 2026-10-01\n\
\n\
### Fixed\n\
\n\
- a fix\n\
\n\
## Unreleased\n\
\n\
- not a version section\n\
\n\
## [2.0.0-rc] - 2026-11-02\n\
\n\
- prerelease without a plain triple\n\
";

	#[test]
	fn parse_splits_sections_and_drops_unparsable_headers() {
		let entries = parse_changelog(SAMPLE);
		assert_eq!(entries.len(), 2, "unparsable headers reset the section");
		assert_eq!(entries[0].major, 1);
		assert_eq!(entries[0].minor, 2);
		assert_eq!(entries[0].patch, 3);
		assert!(entries[0].content.starts_with("## [1.2.3] - 2026-11-01"));
		assert!(entries[0].content.contains("- first feature"));
		assert!(entries[0].content.ends_with("- second feature"));
		assert_eq!(entries[1].patch, 2);
		assert!(entries[1].content.contains("- a fix"));
	}

	#[test]
	fn markdown_reverses_the_file_order_like_upstream() {
		// Upstream `handleChangelogCommand` renders `allEntries.reverse()` —
		// the file's newest-first order flips, so the oldest parsed entry
		// lands first in the "What's New" block.
		let markdown = changelog_markdown_from(SAMPLE);
		let first = markdown.find("## [1.2.2]").expect("1.2.2 present");
		let second = markdown.find("## [1.2.3]").expect("1.2.3 present");
		assert!(first < second, "reversed file order: {markdown}");
	}

	#[test]
	fn empty_input_reports_no_entries() {
		assert_eq!(changelog_markdown_from(""), "No changelog entries found.");
		assert_eq!(
			changelog_markdown_from("# only a title\n"),
			"No changelog entries found."
		);
	}

	#[test]
	fn embedded_changelog_parses() {
		let entries = parse_changelog(EMBEDDED_CHANGELOG);
		assert!(
			!entries.is_empty(),
			"the repo CHANGELOG.md keeps at least one version section"
		);
	}
}
