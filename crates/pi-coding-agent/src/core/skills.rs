//! Agent skills — port of `pi/packages/coding-agent/src/core/skills.ts`.
//!
//! Discovery rules (per directory): a directory containing `SKILL.md` is a
//! skill root — load it and stop recursing; otherwise root-level `*.md`
//! children count as skills when they carry a description, and bare
//! subdirectories are recursed looking for `SKILL.md`. Dotfiles,
//! `node_modules`, and paths ignored by `.gitignore`/`.ignore`/`.fdignore`
//! (rules re-prefixed relative to the scan root, like upstream) are skipped.
//! Name collisions keep the first loader order winner (first-wins) and
//! produce a collision diagnostic; files already loaded through one path are
//! skipped silently when reached again through a symlink.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::diagnostics::ResourceDiagnostic;
use crate::core::frontmatter::parse_frontmatter;

/// Max name length per spec.
const MAX_NAME_LENGTH: usize = 64;
/// Max description length per spec.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// Where a skill came from (`sourceInfo` upstream, reduced to the scope tag
/// the interactive autocomplete prefixes descriptions with).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
	User,
	Project,
	Path,
}

impl SkillSource {
	pub fn as_str(&self) -> &'static str {
		match self {
			SkillSource::User => "user",
			SkillSource::Project => "project",
			SkillSource::Path => "path",
		}
	}

	/// Upstream `getAutocompleteSourceTag` scope prefix for synthetic local
	/// sources: `u` (user), `p` (project), `t` (explicit path).
	pub fn tag(&self) -> &'static str {
		match self {
			SkillSource::User => "u",
			SkillSource::Project => "p",
			SkillSource::Path => "t",
		}
	}
}

#[derive(Debug, Clone)]
pub struct Skill {
	pub name: String,
	pub description: String,
	pub file_path: PathBuf,
	pub base_dir: PathBuf,
	pub source: SkillSource,
	pub disable_model_invocation: bool,
}

/// A loaded skill file plus its diagnostics (upstream `loadSkillFromFile`).
struct LoadedSkill {
	skill: Option<Skill>,
	diagnostics: Vec<ResourceDiagnostic>,
}

pub struct LoadSkillsFromDirOptions<'a> {
	pub dir: &'a Path,
	pub source: SkillSource,
}

#[derive(Debug)]
pub struct LoadSkillsResult {
	pub skills: Vec<Skill>,
	pub diagnostics: Vec<ResourceDiagnostic>,
}

/// Accumulated gitignore-style rules (one matcher per upstream, rules
/// re-prefixed relative to the scan root as directories are visited).
struct IgnoreRules {
	lines: Vec<String>,
	matcher: ignore::gitignore::Gitignore,
}

impl IgnoreRules {
	fn new() -> Self {
		Self {
			lines: Vec::new(),
			matcher: ignore::gitignore::Gitignore::empty(),
		}
	}

	/// Read this directory's ignore files, re-prefix their rules relative to
	/// the scan root, and rebuild the matcher (upstream `addIgnoreRules` on a
	/// single shared `ignore()` instance).
	fn add_dir_rules(&mut self, dir: &Path, root: &Path) {
		let relative_dir = relative_posix(root, dir);
		let prefix = if relative_dir.is_empty() {
			String::new()
		} else {
			format!("{relative_dir}/")
		};
		for filename in IGNORE_FILE_NAMES {
			let Ok(content) = std::fs::read_to_string(dir.join(filename)) else {
				continue;
			};
			for line in content
				.split('\n')
				.map(|line| line.strip_suffix('\r').unwrap_or(line))
			{
				let Some(pattern) = prefix_ignore_pattern(line, &prefix) else {
					continue;
				};
				self.lines.push(pattern);
			}
		}
		let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
		for line in &self.lines {
			let _ = builder.add_line(Some(root.to_path_buf()), line);
		}
		if let Ok(matcher) = builder.build() {
			self.matcher = matcher;
		}
	}

	fn ignores(&self, rel_path: &str, is_dir: bool) -> bool {
		matches!(
			self.matcher.matched(Path::new(rel_path), is_dir),
			ignore::Match::Ignore(_)
		)
	}
}

/// Upstream `prefixIgnorePattern`: blank lines and comments drop out, `!`
/// negation and `\!`/`/` escapes normalize, and the rule gains the
/// directory's relative prefix so one root-anchored matcher serves every
/// nested ignore file.
fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
	let trimmed = line.trim();
	if trimmed.is_empty() {
		return None;
	}
	if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
		return None;
	}

	let mut pattern = line;
	let mut negated = false;
	if let Some(rest) = pattern.strip_prefix('!') {
		negated = true;
		pattern = rest;
	} else if let Some(rest) = pattern.strip_prefix("\\!") {
		pattern = rest;
	}
	let pattern = pattern.strip_prefix('/').unwrap_or(pattern);

	let prefixed = if prefix.is_empty() {
		pattern.to_string()
	} else {
		format!("{prefix}{pattern}")
	};
	Some(if negated {
		format!("!{prefixed}")
	} else {
		prefixed
	})
}

/// Node `path.relative(root, path)` for the only shape the walker produces:
/// `path` at or below `root`. Posix separators.
fn relative_posix(root: &Path, path: &Path) -> String {
	let Ok(rel) = path.strip_prefix(root) else {
		return path.to_string_lossy().into_owned();
	};
	rel.components()
		.map(|c| c.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/")
}

/// Validate skill name per Agent Skills spec; each violation is a warning.
fn validate_name(name: &str) -> Vec<String> {
	let mut errors = Vec::new();
	if name.chars().count() > MAX_NAME_LENGTH {
		errors.push(format!(
			"name exceeds {MAX_NAME_LENGTH} characters ({})",
			name.chars().count()
		));
	}
	if !name
		.chars()
		.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
	{
		errors.push(
			"name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
		);
	}
	if name.starts_with('-') || name.ends_with('-') {
		errors.push("name must not start or end with a hyphen".into());
	}
	if name.contains("--") {
		errors.push("name must not contain consecutive hyphens".into());
	}
	errors
}

/// Validate description per Agent Skills spec.
fn validate_description(description: Option<&str>) -> Vec<String> {
	match description {
		None => vec!["description is required".to_string()],
		Some(text) if text.trim().is_empty() => {
			vec!["description is required".to_string()]
		}
		Some(text) if text.chars().count() > MAX_DESCRIPTION_LENGTH => vec![format!(
			"description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
			text.chars().count()
		)],
		Some(_) => Vec::new(),
	}
}

/// Load skills from a directory (upstream `loadSkillsFromDir`).
pub fn load_skills_from_dir(options: LoadSkillsFromDirOptions<'_>) -> LoadSkillsResult {
	let mut rules = IgnoreRules::new();
	let (skills, diagnostics) =
		load_skills_from_dir_internal(options.dir, options.source, true, &mut rules, options.dir);
	LoadSkillsResult {
		skills,
		diagnostics,
	}
}

fn load_skills_from_dir_internal(
	dir: &Path,
	source: SkillSource,
	include_root_files: bool,
	rules: &mut IgnoreRules,
	root: &Path,
) -> (Vec<Skill>, Vec<ResourceDiagnostic>) {
	let mut skills = Vec::new();
	let mut diagnostics = Vec::new();

	if !dir.exists() {
		return (skills, diagnostics);
	}

	rules.add_dir_rules(dir, root);

	let Ok(entries) = std::fs::read_dir(dir) else {
		return (skills, diagnostics);
	};
	// Node returns OS order; sorting gives deterministic first-wins across
	// platforms (declared deviation, see tests).
	let mut entries: Vec<_> = entries.flatten().collect();
	entries.sort_by_key(|entry| entry.file_name());

	// A SKILL.md makes this directory a skill root: load the first
	// non-ignored one and do not recurse.
	for entry in &entries {
		if entry.file_name() != "SKILL.md" {
			continue;
		}
		let full_path = entry.path();
		let is_file = matches!(resolve_entry_kind(entry), Some(EntryKind::File));
		if !is_file {
			continue;
		}
		let rel_path = relative_posix(root, &full_path);
		if rules.ignores(&rel_path, false) {
			continue;
		}
		let result = load_skill_from_file(&full_path, source);
		if let Some(skill) = result.skill {
			skills.push(skill);
		}
		diagnostics.extend(result.diagnostics);
		return (skills, diagnostics);
	}

	for entry in &entries {
		let name = entry.file_name().to_string_lossy().into_owned();
		if name.starts_with('.') || name == "node_modules" {
			continue;
		}
		let full_path = entry.path();
		match resolve_entry_kind(entry) {
			None => continue,
			Some(kind) => {
				let rel_path = relative_posix(root, &full_path);
				if rules.ignores(&rel_path, kind.is_dir()) {
					continue;
				}
				match kind {
					EntryKind::Dir => {
						let (sub_skills, sub_diagnostics) =
							load_skills_from_dir_internal(&full_path, source, false, rules, root);
						skills.extend(sub_skills);
						diagnostics.extend(sub_diagnostics);
					}
					EntryKind::File => {
						if !include_root_files || !name.ends_with(".md") {
							continue;
						}
						let result = load_skill_from_file(&full_path, source);
						if let Some(skill) = result.skill {
							skills.push(skill);
						}
						diagnostics.extend(result.diagnostics);
					}
				}
			}
		}
	}

	(skills, diagnostics)
}

/// Entry kind with symlinks resolved to their target (upstream `statSync`
/// fallbacks); `None` for broken links and specials.
enum EntryKind {
	File,
	Dir,
}

impl EntryKind {
	fn is_dir(&self) -> bool {
		matches!(self, EntryKind::Dir)
	}
}

fn resolve_entry_kind(entry: &std::fs::DirEntry) -> Option<EntryKind> {
	let file_type = entry.file_type().ok()?;
	if file_type.is_symlink() {
		let metadata = std::fs::metadata(entry.path()).ok()?;
		return if metadata.is_dir() {
			Some(EntryKind::Dir)
		} else if metadata.is_file() {
			Some(EntryKind::File)
		} else {
			None
		};
	}
	if file_type.is_dir() {
		Some(EntryKind::Dir)
	} else if file_type.is_file() {
		Some(EntryKind::File)
	} else {
		None
	}
}

/// Load one skill file: frontmatter validation, name fallback to the parent
/// directory name, and spec warnings (the skill still loads with warnings
/// unless the description is missing or empty). Non-`SKILL.md` markdown only
/// counts when it has a description.
fn load_skill_from_file(file_path: &Path, source: SkillSource) -> LoadedSkill {
	let mut diagnostics = Vec::new();
	let is_declared_skill = file_path.file_name().is_some_and(|n| n == "SKILL.md");

	let raw_content = match std::fs::read_to_string(file_path) {
		Ok(content) => content,
		Err(error) => {
			diagnostics.push(ResourceDiagnostic::warning(
				error.to_string(),
				file_path.display().to_string(),
			));
			return LoadedSkill {
				skill: None,
				diagnostics,
			};
		}
	};

	let frontmatter = match parse_frontmatter(&raw_content) {
		Ok(frontmatter) => frontmatter,
		Err(error) => {
			if is_declared_skill {
				diagnostics.push(ResourceDiagnostic::warning(
					error.to_string(),
					file_path.display().to_string(),
				));
			}
			return LoadedSkill {
				skill: None,
				diagnostics,
			};
		}
	};

	let description = frontmatter.get_str("description");
	let has_description = description.is_some_and(|d| !d.trim().is_empty());
	if !is_declared_skill && !has_description {
		return LoadedSkill {
			skill: None,
			diagnostics,
		};
	}

	for error in validate_description(description) {
		diagnostics.push(ResourceDiagnostic::warning(
			error,
			file_path.display().to_string(),
		));
	}

	// Name from frontmatter, falling back to the parent directory name (an
	// empty frontmatter name falls through, like the upstream falsy check).
	let skill_dir = file_path.parent().unwrap_or(Path::new(""));
	let parent_dir_name = skill_dir
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_default();
	let name = frontmatter
		.get_str("name")
		.filter(|n| !n.is_empty())
		.map(str::to_string)
		.unwrap_or(parent_dir_name);

	for error in validate_name(&name) {
		diagnostics.push(ResourceDiagnostic::warning(
			error,
			file_path.display().to_string(),
		));
	}

	if !has_description {
		return LoadedSkill {
			skill: None,
			diagnostics,
		};
	}

	LoadedSkill {
		skill: Some(Skill {
			name,
			description: description.unwrap_or_default().to_string(),
			file_path: file_path.to_path_buf(),
			base_dir: skill_dir.to_path_buf(),
			source,
			disable_model_invocation: frontmatter.get_bool("disable-model-invocation")
				== Some(true),
		}),
		diagnostics,
	}
}

/// Which tool reads skill files in the prompt copy (upstream `fileReadTool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileReadTool {
	Read,
	Bash,
	Indirect,
}

/// Format skills for inclusion in a system prompt (upstream
/// `formatSkillsForPrompt`, Agent Skills XML). Skills with
/// `disable-model-invocation` are excluded; an empty visible set yields "".
pub fn format_skills_for_prompt(skills: &[Skill], file_read_tool: FileReadTool) -> String {
	let visible: Vec<&Skill> = skills
		.iter()
		.filter(|skill| !skill.disable_model_invocation)
		.collect();
	if visible.is_empty() {
		return String::new();
	}

	let mut lines = vec![
		"\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
		match file_read_tool {
			FileReadTool::Read => {
				"Use the read tool to load a skill's file when the task matches its description."
					.to_string()
			}
			FileReadTool::Bash => {
				"Use bash to load a skill's file when the task matches its description.".to_string()
			}
			FileReadTool::Indirect => {
				"Load a skill's file when the task matches its description.".to_string()
			}
		},
		"When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
		String::new(),
		"<available_skills>".to_string(),
	];

	for skill in visible {
		lines.push("  <skill>".to_string());
		lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
		lines.push(format!(
			"    <description>{}</description>",
			escape_xml(&skill.description)
		));
		lines.push(format!(
			"    <location>{}</location>",
			escape_xml(&skill.file_path.display().to_string())
		));
		lines.push("  </skill>".to_string());
	}

	lines.push("</available_skills>".to_string());
	lines.join("\n")
}

fn escape_xml(input: &str) -> String {
	input
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
		.replace('\'', "&apos;")
}

pub struct LoadSkillsOptions {
	/// Working directory for project-local skills.
	pub cwd: PathBuf,
	/// Agent config directory for global skills.
	pub agent_dir: PathBuf,
	/// Explicit skill paths (files or directories).
	pub skill_paths: Vec<String>,
	/// Include default skills directories.
	pub include_defaults: bool,
}

/// `canonicalizePath` (utils/paths.ts): realpath when reachable, the input
/// otherwise.
fn canonicalize_path(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Component-wise containment (upstream `isUnderPath`'s separator-prefix
/// check; `Path::starts_with` matches whole components, never byte prefixes).
fn is_under_path(target: &Path, root: &Path) -> bool {
	target == root || target.starts_with(root)
}

/// Name-collision and symlink dedup state for the aggregate loader (upstream
/// `loadSkills`'s `skillMap`/`realPathSet`/`collisionDiagnostics`).
#[derive(Default)]
struct SkillMerger {
	skills: Vec<Skill>,
	index: HashMap<String, usize>,
	real_paths: HashSet<PathBuf>,
	collision_diagnostics: Vec<ResourceDiagnostic>,
}

impl SkillMerger {
	fn add(&mut self, loaded: Vec<Skill>) {
		for skill in loaded {
			let real_path = canonicalize_path(&skill.file_path);
			// The same exact file reached again (via symlink) is skipped
			// silently; a different file with the same name is a collision
			// and the later skill loses.
			if self.real_paths.contains(&real_path) {
				continue;
			}
			match self.index.get(&skill.name) {
				Some(&winner) => self
					.collision_diagnostics
					.push(ResourceDiagnostic::Collision {
						message: format!("name \"{}\" collision", skill.name),
						path: Some(skill.file_path.display().to_string()),
						collision: crate::core::diagnostics::ResourceCollision {
							resource_type: "skill",
							name: skill.name.clone(),
							winner_path: self.skills[winner].file_path.display().to_string(),
							loser_path: skill.file_path.display().to_string(),
						},
					}),
				None => {
					self.index.insert(skill.name.clone(), self.skills.len());
					self.real_paths.insert(real_path);
					self.skills.push(skill);
				}
			}
		}
	}
}

/// Load skills from all configured locations: user `agentDir/skills`, then
/// project `cwd/.pi/skills` (defaults only), then explicit paths. Same-name
/// skills keep the first loader order winner; identical files reached via
/// symlink dedupe silently.
pub fn load_skills(options: LoadSkillsOptions) -> LoadSkillsResult {
	let user_skills_dir = options.agent_dir.join("skills");
	let project_skills_dir = crate::config::project_pi_dir(&options.cwd).join("skills");

	let mut merger = SkillMerger::default();
	let mut all_diagnostics: Vec<ResourceDiagnostic> = Vec::new();

	if options.include_defaults {
		for (dir, source) in [
			(user_skills_dir.clone(), SkillSource::User),
			(project_skills_dir.clone(), SkillSource::Project),
		] {
			let result = load_skills_from_dir(LoadSkillsFromDirOptions { dir: &dir, source });
			all_diagnostics.extend(result.diagnostics);
			merger.add(result.skills);
		}
	}

	let get_source = |resolved_path: &Path| -> SkillSource {
		if !options.include_defaults {
			if is_under_path(resolved_path, &user_skills_dir) {
				return SkillSource::User;
			}
			if is_under_path(resolved_path, &project_skills_dir) {
				return SkillSource::Project;
			}
		}
		SkillSource::Path
	};

	for raw_path in &options.skill_paths {
		let trimmed = raw_path.trim();
		let resolved_path = resolve_against_cwd(Path::new(trimmed), &options.cwd);
		if !resolved_path.exists() {
			all_diagnostics.push(ResourceDiagnostic::warning(
				"skill path does not exist",
				resolved_path.display().to_string(),
			));
			continue;
		}
		let metadata = match std::fs::metadata(&resolved_path) {
			// Upstream's catch-all reports the raw error text.
			Ok(metadata) => metadata,
			Err(error) => {
				all_diagnostics.push(ResourceDiagnostic::warning(
					error.to_string(),
					resolved_path.display().to_string(),
				));
				continue;
			}
		};
		let source = get_source(&resolved_path);
		if metadata.is_dir() {
			let result = load_skills_from_dir(LoadSkillsFromDirOptions {
				dir: &resolved_path,
				source,
			});
			all_diagnostics.extend(result.diagnostics);
			merger.add(result.skills);
		} else if metadata.is_file() && resolved_path.to_string_lossy().ends_with(".md") {
			let result = load_skill_from_file(&resolved_path, source);
			merger.add(result.skill.into_iter().collect());
			all_diagnostics.extend(result.diagnostics);
		} else {
			all_diagnostics.push(ResourceDiagnostic::warning(
				"skill path is not a markdown file",
				resolved_path.display().to_string(),
			));
		}
	}

	let SkillMerger {
		skills,
		collision_diagnostics,
		..
	} = merger;
	all_diagnostics.extend(collision_diagnostics);
	LoadSkillsResult {
		skills,
		diagnostics: all_diagnostics,
	}
}

/// `resolvePath(input, cwd)` for explicit CLI paths: absolute stays, relative
/// joins the cwd. Lexical only (no symlink resolution).
fn resolve_against_cwd(path: &Path, cwd: &Path) -> PathBuf {
	if path.is_absolute() {
		path.to_path_buf()
	} else {
		cwd.join(path)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp_root(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-skills-{tag}-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn skill_md(dir: &Path, name: &str, description: &str) -> PathBuf {
		std::fs::create_dir_all(dir).unwrap();
		let file = dir.join("SKILL.md");
		std::fs::write(
			&file,
			format!("---\nname: {name}\ndescription: {description}\n---\nBody of {name}.\n"),
		)
		.unwrap();
		file
	}

	// ---------------- discovery ----------------

	#[test]
	fn skill_root_stops_recursion_but_sibling_dirs_are_found() {
		let root = temp_root("recurse");
		// Nested skill: root/nested/SKILL.md with a decoy deeper down.
		let nested = root.join("nested");
		skill_md(&nested, "nested", "nested skill");
		skill_md(&nested.join("deeper"), "deeper", "must not load");
		// Sibling skill at root/other/SKILL.md.
		skill_md(&root.join("other"), "other", "other skill");

		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Project,
		});
		let mut names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
		names.sort_unstable();
		assert_eq!(names, ["nested", "other"]);
		assert!(result.diagnostics.is_empty());
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn root_markdown_files_need_description() {
		let root = temp_root("rootmd");
		std::fs::write(
			root.join("described.md"),
			"---\ndescription: has one\n---\nbody",
		)
		.unwrap();
		std::fs::write(root.join("bare.md"), "---\nname: bare\n---\nbody").unwrap();
		// Undeclared skill with a description loads. Upstream names it after
		// the parent DIRECTORY (skills.ts falls back to `basename(dirname)`,
		// not the file stem), so a root-level file inherits the scan root's
		// name unless frontmatter provides one.
		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::User,
		});
		assert_eq!(result.skills.len(), 1);
		let root_name = root.file_name().unwrap().to_string_lossy().into_owned();
		assert_eq!(result.skills[0].name, root_name);
		assert_eq!(result.skills[0].description, "has one");
		assert_eq!(result.skills[0].source, SkillSource::User);
		assert!(result.diagnostics.is_empty());
		// A nested plain .md is likewise named after its containing
		// directory (`sub/tool.md` becomes skill `sub`).
		let sub = root.join("sub");
		std::fs::create_dir_all(&sub).unwrap();
		std::fs::write(sub.join("tool.md"), "---\ndescription: nested\n---\n").unwrap();
		let nested = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &sub,
			source: SkillSource::User,
		});
		assert_eq!(nested.skills[0].name, "sub");
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn dotfiles_node_modules_and_missing_dirs_are_skipped() {
		let root = temp_root("skips");
		skill_md(&root.join(".hidden"), "hidden", "nope");
		skill_md(&root.join("node_modules/dep"), "dep", "nope");
		// A root-level dotfile markdown is also skipped.
		std::fs::write(root.join(".dotskill.md"), "---\ndescription: nope\n---\n").unwrap();

		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Path,
		});
		assert!(result.skills.is_empty(), "{:?}", result.skills);
		// Missing directories return empty without error.
		let missing = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root.join("nope"),
			source: SkillSource::Path,
		});
		assert!(missing.skills.is_empty() && missing.diagnostics.is_empty());
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn gitignore_rules_apply_with_relative_prefixes() {
		let root = temp_root("gitignore");
		skill_md(&root.join("ignored-dir"), "ignored-dir", "nope");
		skill_md(&root.join("kept"), "kept", "yes");
		skill_md(&root.join("sub/skipped"), "skipped", "nope");
		skill_md(&root.join("sub/kept"), "sub-kept", "yes");
		// Ignore files at two depths: root ignores the whole `ignored-dir/`
		// and `*.tmp.md`; `sub/.gitignore` re-anchors `/skipped` to itself.
		std::fs::write(root.join(".gitignore"), "ignored-dir/\n*.tmp.md\n").unwrap();
		std::fs::write(root.join("sub/.gitignore"), "/skipped\n").unwrap();
		std::fs::write(root.join("loose.tmp.md"), "---\ndescription: nope\n---\n").unwrap();

		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Project,
		});
		let mut names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
		names.sort_unstable();
		assert_eq!(names, ["kept", "sub-kept"], "names: {names:?}");
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn negated_gitignore_pattern_unignores_a_skill() {
		let root = temp_root("negation");
		skill_md(&root.join("vendor/keepme"), "keepme", "yes");
		std::fs::write(
			root.join(".gitignore"),
			"vendor\n!vendor/\n!vendor/keepme\n",
		)
		.unwrap();
		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Path,
		});
		let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
		assert_eq!(names, ["keepme"], "names: {names:?}");
		std::fs::remove_dir_all(root).unwrap();
	}

	// ---------------- validation ----------------

	#[test]
	fn missing_description_drops_the_skill_declared_and_plain() {
		let root = temp_root("nodesc");
		let declared = skill_md(&root.join("no-desc"), "no-desc", "");
		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Path,
		});
		assert!(result.skills.is_empty());
		assert!(
			result
				.diagnostics
				.iter()
				.any(|d| d.message() == "description is required")
		);

		// Plain .md without description: silently skipped, no diagnostics.
		let plain_root = temp_root("nodesc-plain");
		std::fs::write(plain_root.join("plain.md"), "---\nname: plain\n---\n").unwrap();
		let plain = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &plain_root,
			source: SkillSource::Path,
		});
		assert!(plain.skills.is_empty() && plain.diagnostics.is_empty());
		std::fs::remove_dir_all(root).unwrap();
		std::fs::remove_dir_all(plain_root).unwrap();
		assert_eq!(declared.file_name(), Some(std::ffi::OsStr::new("SKILL.md")));
	}

	#[test]
	fn invalid_name_still_loads_with_warning() {
		let root = temp_root("badname");
		skill_md(&root.join("weird-name"), "Weird_Name", "loads anyway");
		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Path,
		});
		assert_eq!(result.skills.len(), 1);
		assert!(
			result
				.diagnostics
				.iter()
				.any(|d| d.message().contains("invalid characters"))
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn bad_yaml_in_skill_md_is_a_warning_and_not_loaded() {
		let root = temp_root("badyaml");
		let file = root.join("broken").join("SKILL.md");
		std::fs::create_dir_all(file.parent().unwrap()).unwrap();
		std::fs::write(&file, "---\ndescription: [unclosed\n---\nbody").unwrap();
		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Path,
		});
		assert!(result.skills.is_empty());
		assert_eq!(result.diagnostics.len(), 1);
		// The exact wording is serde_yaml's; only the warning shape (message
		// + path) is contractual.
		assert!(!result.diagnostics[0].message().is_empty());
		// The same bad YAML in a plain .md produces nothing at all (fresh
		// scan root, so the SKILL.md warning above does not leak in).
		std::fs::write(root.join("broken.md"), "---\ndescription: [unclosed\n---\n").unwrap();
		let plain_root = root.join("plain-root");
		std::fs::create_dir_all(&plain_root).unwrap();
		std::fs::write(
			plain_root.join("broken.md"),
			"---\ndescription: [unclosed\n---\n",
		)
		.unwrap();
		let plain = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &plain_root,
			source: SkillSource::Path,
		});
		assert!(plain.skills.is_empty() && plain.diagnostics.is_empty());
		std::fs::remove_dir_all(root).unwrap();
	}

	// ---------------- aggregation ----------------

	#[test]
	fn name_collision_is_first_wins_with_collision_diagnostic() {
		let root = temp_root("collision");
		let agent_dir = root.join("agent");
		let project = root.join("project");
		std::fs::create_dir_all(&agent_dir).unwrap();
		std::fs::create_dir_all(&project).unwrap();
		let first = skill_md(&agent_dir.join("skills/first-dir"), "dup", "first");
		let second = skill_md(&project.join(".pi/skills/second-dir"), "dup", "second");

		let result = load_skills(LoadSkillsOptions {
			cwd: project.clone(),
			agent_dir: agent_dir.clone(),
			skill_paths: vec![],
			include_defaults: true,
		});
		assert_eq!(result.skills.len(), 1);
		assert_eq!(result.skills[0].description, "first");
		assert_eq!(result.skills[0].file_path, first);
		assert_eq!(result.diagnostics.len(), 1);
		match &result.diagnostics[0] {
			ResourceDiagnostic::Collision {
				message, collision, ..
			} => {
				assert_eq!(message, "name \"dup\" collision");
				assert_eq!(collision.winner_path, first.display().to_string());
				assert_eq!(collision.loser_path, second.display().to_string());
				assert_eq!(collision.resource_type, "skill");
			}
			other => panic!("expected collision, got {other:?}"),
		}
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn load_skills_merges_defaults_and_paths_in_order() {
		let root = temp_root("merge");
		let agent_dir = root.join("agent");
		let project = root.join("project");
		std::fs::create_dir_all(&agent_dir).unwrap();
		std::fs::create_dir_all(&project).unwrap();
		skill_md(
			&agent_dir.join("skills/from-user"),
			"from-user",
			"user desc",
		);
		skill_md(
			&project.join(".pi/skills/from-project"),
			"from-project",
			"project desc",
		);

		let result = load_skills(LoadSkillsOptions {
			cwd: project.clone(),
			agent_dir: agent_dir.clone(),
			skill_paths: vec![],
			include_defaults: true,
		});
		assert_eq!(
			result
				.skills
				.iter()
				.map(|s| s.name.as_str())
				.collect::<Vec<_>>(),
			vec!["from-user", "from-project"]
		);
		assert_eq!(result.skills[0].source, SkillSource::User);
		assert_eq!(result.skills[1].source, SkillSource::Project);

		// include_defaults=false: a path under the skills dirs still tags
		// user/project, everything else is "path".
		let tagged = load_skills(LoadSkillsOptions {
			cwd: project.clone(),
			agent_dir: agent_dir.clone(),
			skill_paths: vec![
				project
					.join(".pi/skills/from-project")
					.to_string_lossy()
					.into_owned(),
				agent_dir
					.join("skills/from-user")
					.to_string_lossy()
					.into_owned(),
			],
			include_defaults: false,
		});
		assert_eq!(
			tagged
				.skills
				.iter()
				.map(|s| (s.name.as_str(), s.source))
				.collect::<Vec<_>>(),
			vec![
				("from-project", SkillSource::Project),
				("from-user", SkillSource::User)
			]
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn explicit_paths_report_missing_and_non_markdown() {
		let root = temp_root("paths");
		let agent_dir = root.join("agent");
		std::fs::create_dir_all(&agent_dir).unwrap();
		let plain_file = root.join("not-a-skill.txt");
		std::fs::write(&plain_file, "nope").unwrap();

		let result = load_skills(LoadSkillsOptions {
			cwd: root.clone(),
			agent_dir: agent_dir.clone(),
			skill_paths: vec![
				root.join("missing-dir").to_string_lossy().into_owned(),
				plain_file.to_string_lossy().into_owned(),
			],
			include_defaults: true,
		});
		assert!(result.skills.is_empty());
		let messages: Vec<_> = result.diagnostics.iter().map(|d| d.message()).collect();
		assert!(messages.contains(&"skill path does not exist"));
		assert!(messages.contains(&"skill path is not a markdown file"));
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn symlinked_skill_file_dedupes_silently() {
		let root = temp_root("symlink");
		let agent_dir = root.join("agent");
		let project = root.join("project");
		std::fs::create_dir_all(&agent_dir).unwrap();
		std::fs::create_dir_all(&project).unwrap();
		let real = skill_md(&agent_dir.join("skills/real"), "twin", "one file");
		std::fs::create_dir_all(project.join(".pi/skills")).unwrap();
		let link = project.join(".pi/skills/twin-dir");
		std::os::unix::fs::symlink(real.parent().unwrap(), &link).unwrap();

		let result = load_skills(LoadSkillsOptions {
			cwd: project.clone(),
			agent_dir: agent_dir.clone(),
			skill_paths: vec![],
			include_defaults: true,
		});
		assert_eq!(result.skills.len(), 1, "{:?}", result.skills);
		assert!(result.diagnostics.is_empty(), "{result:?}");
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn symlinked_directory_target_is_followed_for_skills() {
		let root = temp_root("symdir");
		let target = skill_md(&root.join("actual"), "linked", "via symlink");
		let link = root.join("link");
		std::os::unix::fs::symlink(target.parent().unwrap(), &link).unwrap();
		let result = load_skills_from_dir(LoadSkillsFromDirOptions {
			dir: &root,
			source: SkillSource::Path,
		});
		let names: Vec<_> = result.skills.iter().map(|s| s.name.as_str()).collect();
		assert!(names.contains(&"linked"), "{names:?}");
		std::fs::remove_dir_all(root).unwrap();
	}

	// ---------------- format ----------------

	fn sample_skill(name: &str, description: &str) -> Skill {
		Skill {
			name: name.into(),
			description: description.into(),
			file_path: PathBuf::from("/tmp/skills/demo/SKILL.md"),
			base_dir: PathBuf::from("/tmp/skills/demo"),
			source: SkillSource::User,
			disable_model_invocation: false,
		}
	}

	#[test]
	fn format_matches_upstream_xml_verbatim() {
		let skill = sample_skill("demo", "A demo skill");
		let formatted = format_skills_for_prompt(std::slice::from_ref(&skill), FileReadTool::Read);
		let expected = "\n\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool to load a skill's file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>\n  <skill>\n    <name>demo</name>\n    <description>A demo skill</description>\n    <location>/tmp/skills/demo/SKILL.md</location>\n  </skill>\n</available_skills>";
		assert_eq!(formatted, expected);

		let bash = format_skills_for_prompt(std::slice::from_ref(&skill), FileReadTool::Bash);
		assert!(
			bash.contains("Use bash to load a skill's file when the task matches its description.")
		);
		let indirect = format_skills_for_prompt(&[skill], FileReadTool::Indirect);
		assert!(indirect.contains("Load a skill's file when the task matches its description."));
	}

	#[test]
	fn format_escapes_xml_entities_in_order() {
		let skill = sample_skill("esc&<test>", "quotes \" and ' plus <tag> & more");
		let formatted = format_skills_for_prompt(&[skill], FileReadTool::Read);
		assert!(formatted.contains("<name>esc&amp;&lt;test&gt;</name>"));
		assert!(formatted.contains(
			"<description>quotes &quot; and &apos; plus &lt;tag&gt; &amp; more</description>"
		));
	}

	#[test]
	fn format_excludes_disabled_and_empty_set_is_empty_string() {
		let mut hidden = sample_skill("hidden", "only via /skill:");
		hidden.disable_model_invocation = true;
		let visible = sample_skill("visible", "shown");
		assert_eq!(
			format_skills_for_prompt(&[hidden.clone()], FileReadTool::Read),
			""
		);
		assert_eq!(format_skills_for_prompt(&[], FileReadTool::Read), "");
		let both = format_skills_for_prompt(&[hidden, visible], FileReadTool::Read);
		assert!(both.contains("<name>visible</name>"));
		assert!(!both.contains("<name>hidden</name>"));
	}
}
