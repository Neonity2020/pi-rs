//! Package manager — port of
//! `pi/packages/coding-agent/src/core/package-manager.ts`, reduced to the
//! package CLI surface: npm/git/local source parsing, install/remove/update
//! against `<agent-dir>/npm|git` (project scope `.pi/npm|git`, trust-gated),
//! `packages` persistence in `settings.json`, and resource discovery from
//! installed packages (extensions/skills/prompts/themes).
//!
//! Declared reductions from upstream: settings `packages` entries are read
//! as strings (object-form `autoload`/resource filters are ignored for
//! selection but their `source` is honored for listing); per-resource
//! settings arrays (`extensions`/`skills`/…) are not resolved here; the npm
//! tool is npm itself (`npmCommand` replaces the command, pnpm/bun flag
//! variants are not modeled); legacy global npm layouts and the temporary
//! scope are not resolved; startup resolve reports missing packages as
//! warnings instead of interactive auto-install; git update uses fetch +
//! compare + `reset --hard` with dependency reinstall but not the update
//! marker's clean-reinstall ladder.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::skills::{IgnoreRules, relative_posix};

/// Network-facing child process budget (upstream `NETWORK_TIMEOUT_MS`).
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
/// Install/remove budget (upstream `INSTALL_TIMEOUT_MS`-family).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// One parsed package source — upstream `ParsedSource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSource {
	/// `npm:<name>@<version|range>` — `spec` is what npm consumes.
	Npm {
		spec: String,
		name: String,
		version: Option<String>,
		range: Option<String>,
		/// An exact version fixes the install (never auto-updated).
		pinned: bool,
	},
	/// A git URL (https/scp-like/shorthand), optionally `@ref`-pinned.
	Git {
		repo: String,
		host: String,
		path: String,
		ref_: Option<String>,
		pinned: bool,
	},
	/// A filesystem path; content is used in place, nothing is installed.
	Local { path: String },
}

impl ParsedSource {
	/// Stable identity for settings dedupe and update targeting — upstream
	/// `getPackageIdentity`.
	pub fn identity(&self, base_dir: Option<&Path>) -> String {
		match self {
			ParsedSource::Npm { name, .. } => format!("npm:{name}"),
			ParsedSource::Git { host, path, .. } => format!("git:{host}/{path}"),
			ParsedSource::Local { path } => {
				let resolved = resolve_path(base_dir.unwrap_or(Path::new(".")), path);
				format!("local:{}", resolved.to_string_lossy().replace('\\', "/"))
			}
		}
	}
}

/// `npm:<spec>` prefix detection plus the npm/`@scope/name@version` grammar —
/// upstream `parseSource`'s npm branch and `parseNpmSpec`.
fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
	// `^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$` — name may contain one `/` (scope),
	// the version is everything after the last top-level `@`.
	let trimmed = spec.trim();
	let (name, version) = match trimmed.find('@') {
		Some(0) => {
			// Scoped name `@scope/pkg` or `@scope/pkg@version`: find the
			// second `@`.
			match trimmed[1..].find('@') {
				Some(index) => {
					let split = index + 1;
					(&trimmed[..split], Some(trimmed[split + 1..].to_string()))
				}
				None => (trimmed, None),
			}
		}
		Some(index) => (&trimmed[..index], Some(trimmed[index + 1..].to_string())),
		None => (trimmed, None),
	};
	(
		name.to_string(),
		version.filter(|version| !version.is_empty()),
	)
}

/// Upstream `valid`/`validRange` via the semver crate: an exact version pins,
/// anything else that parses as a range is the update target.
fn npm_version_fields(version: Option<String>) -> (Option<String>, Option<String>, bool) {
	match version {
		None => (None, None, false),
		Some(version) => {
			if semver::Version::parse(&version).is_ok() {
				(Some(version.clone()), Some(version), true)
			} else if semver::VersionReq::parse(&version).is_ok() {
				(Some(version.clone()), Some(version), false)
			} else {
				// Unparseable: npm still accepts it as a spec; treat as an
				// unpinned install without a range.
				(Some(version), None, false)
			}
		}
	}
}

/// Upstream `splitRef`: peel a trailing `@ref` off scp-like, URL, and
/// shorthand forms.
fn split_git_ref(url: &str) -> (String, Option<String>) {
	if let Some(rest) = url.strip_prefix("git@") {
		if let Some((host, path_with_ref)) = rest.split_once(':') {
			if let Some(index) = path_with_ref.find('@') {
				let repo_path = &path_with_ref[..index];
				let git_ref = &path_with_ref[index + 1..];
				if !repo_path.is_empty() && !git_ref.is_empty() {
					return (format!("git@{host}:{repo_path}"), Some(git_ref.to_string()));
				}
			}
			return (url.to_string(), None);
		}
		return (url.to_string(), None);
	}
	if url.contains("://") {
		if let Ok(mut parsed) = url::Url::parse(url) {
			let path = parsed.path().trim_start_matches('/').to_string();
			if let Some(index) = path.find('@') {
				let repo_path = &path[..index];
				let git_ref = &path[index + 1..];
				if !repo_path.is_empty() && !git_ref.is_empty() {
					parsed.set_path(&format!("/{repo_path}"));
					let repo = parsed.to_string();
					return (
						repo.trim_end_matches('/').to_string(),
						Some(git_ref.to_string()),
					);
				}
			}
			return (url.to_string(), None);
		}
		return (url.to_string(), None);
	}
	// Shorthand `host/path[@ref]`.
	let Some(slash) = url.find('/') else {
		return (url.to_string(), None);
	};
	let (host, path_with_ref) = (&url[..slash], &url[slash + 1..]);
	if let Some(index) = path_with_ref.find('@') {
		let repo_path = &path_with_ref[..index];
		let git_ref = &path_with_ref[index + 1..];
		if !repo_path.is_empty() && !git_ref.is_empty() {
			return (format!("{host}/{repo_path}"), Some(git_ref.to_string()));
		}
	}
	(url.to_string(), None)
}

fn decoded_contains_unsafe_part(value: &str, allow_slash: bool) -> bool {
	let decoded = percent_decode(value);
	for candidate in [value, decoded.as_str()] {
		if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
			return true;
		}
		if !allow_slash && candidate.contains('/') {
			return true;
		}
		if candidate.split('/').any(|part| part == "..") {
			return true;
		}
	}
	false
}

fn percent_decode(value: &str) -> String {
	let bytes = value.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut index = 0;
	while index < bytes.len() {
		match bytes[index] {
			b'%' if index + 2 < bytes.len() => {
				match u8::from_str_radix(&value[index + 1..index + 3], 16) {
					Ok(byte) => {
						out.push(byte);
						index += 3;
					}
					Err(_) => {
						out.push(b'%');
						index += 1;
					}
				}
			}
			byte => {
				out.push(byte);
				index += 1;
			}
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Upstream `parseGitUrl` (utils/git.ts): with a `git:` prefix every
/// shorthand form is accepted (the host needs a dot or `localhost`; the
/// shorthand repo is cloned as `https://…`); without it only explicit
/// protocol URLs (`https?|ssh|git://`) count. Returns `None` otherwise.
pub fn parse_git_url(url: &str) -> Option<ParsedSource> {
	let trimmed = url.trim();
	let has_git_prefix = trimmed.starts_with("git:");
	let body = trimmed.strip_prefix("git:").unwrap_or(trimmed).trim();
	if !has_git_prefix && !is_explicit_protocol_url(body) {
		return None;
	}
	let (repo, git_ref) = split_git_ref(body);
	// Host/path extraction mirrors the three shapes.
	let (host, raw_path, repo) = if let Some(rest) = repo.strip_prefix("git@") {
		let (host, path) = rest.split_once(':')?;
		(host.to_string(), path.to_string(), repo)
	} else if repo
		.strip_prefix("https://")
		.or_else(|| repo.strip_prefix("http://"))
		.or_else(|| repo.strip_prefix("ssh://"))
		.or_else(|| repo.strip_prefix("git://"))
		.is_some()
	{
		let parsed = url::Url::parse(&repo).ok()?;
		let host = parsed.host_str()?.to_string();
		let path = parsed.path().trim_start_matches('/').to_string();
		(host, path, repo)
	} else {
		// Shorthand (git: prefix only): host must be dotted or localhost and
		// the clone URL becomes https.
		let (host, path) = repo.split_once('/')?;
		if !host.contains('.') && host != "localhost" {
			return None;
		}
		(
			host.to_string(),
			path.to_string(),
			format!("https://{repo}"),
		)
	};
	let normalized = raw_path
		.trim_start_matches('/')
		.strip_suffix(".git")
		.unwrap_or(&raw_path)
		.to_string();
	if host.is_empty() || normalized.split('/').count() < 2 {
		return None;
	}
	if decoded_contains_unsafe_part(&host, false) || decoded_contains_unsafe_part(&normalized, true)
	{
		return None;
	}
	let pinned = git_ref.is_some();
	Some(ParsedSource::Git {
		repo,
		host,
		path: normalized,
		ref_: git_ref,
		pinned,
	})
}

fn is_explicit_protocol_url(url: &str) -> bool {
	url.starts_with("https://")
		|| url.starts_with("http://")
		|| url.starts_with("ssh://")
		|| url.starts_with("git://")
}

/// A local path when the source is neither an npm spec nor a git URL —
/// upstream's `isLocalPath` check reduces to "does it exist or look like a
/// path" (no scheme prefix, no npm name grammar).
fn is_local_path(source: &str) -> bool {
	if source.starts_with("npm:") {
		return false;
	}
	if source.starts_with("./")
		|| source.starts_with("../")
		|| source.starts_with('/')
		|| source.starts_with('~')
		|| source.starts_with('.')
	{
		return true;
	}
	// An existing filesystem path is local even without a `./` prefix.
	Path::new(source).exists()
}

/// Upstream `parseSource`.
pub fn parse_source(source: &str) -> ParsedSource {
	if let Some(spec) = source.strip_prefix("npm:") {
		let (name, version) = parse_npm_spec(spec);
		let (version, range, pinned) = npm_version_fields(version);
		return ParsedSource::Npm {
			spec: spec.trim().to_string(),
			name,
			version,
			range,
			pinned,
		};
	}
	if is_local_path(source) {
		return ParsedSource::Local {
			path: source.to_string(),
		};
	}
	if let Some(git) = parse_git_url(source) {
		return git;
	}
	ParsedSource::Local {
		path: source.to_string(),
	}
}

/// Resolve a source-provided path against `base`, mirroring the loader's
/// path helpers (`~` expands, relative joins).
fn resolve_path(base: &Path, path: &str) -> PathBuf {
	let expanded = if let Some(rest) = path.strip_prefix("~/") {
		dirs::home_dir().unwrap_or_default().join(rest)
	} else {
		PathBuf::from(path)
	};
	if expanded.is_absolute() {
		expanded
	} else {
		base.join(expanded)
	}
}

/// One configured package entry — upstream `ConfiguredPackage`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredPackage {
	pub source: String,
	/// `"user"` (global settings) or `"project"`.
	pub scope: &'static str,
	/// Install path of the resolved package, when installed.
	pub installed_path: Option<PathBuf>,
	/// The raw settings entry (string or object) — object entries carry
	/// per-resource pattern arrays and the `autoload` flag.
	pub entry: Value,
}

/// Resource paths discovered inside installed packages — upstream
/// `ResolvedPaths` reduced to the path lists.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackageResources {
	pub extensions: Vec<PathBuf>,
	pub skills: Vec<PathBuf>,
	pub prompts: Vec<PathBuf>,
	pub themes: Vec<PathBuf>,
	/// Non-fatal problems (missing installs, bad manifests).
	pub warnings: Vec<String>,
}

/// `npm install`/`uninstall`/`view` tooling — upstream `getNpmCommand` with
/// the settings `npmCommand` override.
fn npm_command(settings: &serde_json::Map<String, Value>) -> (String, Vec<String>) {
	match settings.get("npmCommand") {
		Some(Value::Array(items)) if !items.is_empty() => {
			let mut items = items.iter().filter_map(Value::as_str);
			let Some(command) = items.next() else {
				return ("npm".to_string(), Vec::new());
			};
			(command.to_string(), items.map(str::to_string).collect())
		}
		_ => ("npm".to_string(), Vec::new()),
	}
}

fn is_offline() -> bool {
	std::env::var("PI_OFFLINE")
		.is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Run a child command to completion, failing with combined stderr/stdout on
/// a non-zero exit (upstream `runCommand`).
async fn run_command(
	command: &str,
	args: &[String],
	cwd: Option<&Path>,
	timeout: Duration,
) -> Result<(), String> {
	run_command_capture(command, args, cwd, timeout)
		.await
		.map(|_| ())
}

async fn run_command_capture(
	command: &str,
	args: &[String],
	cwd: Option<&Path>,
	timeout: Duration,
) -> Result<String, String> {
	let mut cmd = tokio::process::Command::new(command);
	cmd.args(args)
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped());
	if let Some(cwd) = cwd {
		cmd.current_dir(cwd);
	}
	let child = cmd
		.spawn()
		.map_err(|error| format!("failed to spawn {command}: {error}"))?;
	let output = tokio::time::timeout(timeout, child.wait_with_output())
		.await
		.map_err(|_| format!("{command} timed out after {}s", timeout.as_secs()))?
		.map_err(|error| format!("{command}: {error}"))?;
	if output.status.success() {
		Ok(String::from_utf8_lossy(&output.stdout).into_owned())
	} else {
		let detail = String::from_utf8_lossy(&output.stderr);
		let detail = detail.trim();
		Err(if detail.is_empty() {
			format!("{command} exited with {}", output.status)
		} else {
			format!("{command} failed: {detail}")
		})
	}
}

/// The `packages` entries of one settings file, as source strings (object
/// entries contribute their `source`).
fn read_sources(path: &Path) -> Vec<String> {
	read_entries(path)
		.iter()
		.filter_map(|entry| {
			entry.as_str().map(str::to_string).or_else(|| {
				entry
					.get("source")
					.and_then(Value::as_str)
					.map(str::to_string)
			})
		})
		.collect()
}

/// Read-modify-write one settings file's `packages` array, preserving every
/// other key. `edit` returns the new array (or `None` to leave it alone).
fn edit_packages(
	path: &Path,
	edit: impl FnOnce(Vec<Value>) -> Option<Vec<Value>>,
) -> Result<(), String> {
	let text = std::fs::read_to_string(path).unwrap_or_default();
	let mut map = match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
		Ok(Value::Object(map)) => map,
		_ => Default::default(),
	};
	let current = match map.get("packages") {
		Some(Value::Array(items)) => items.clone(),
		_ => Vec::new(),
	};
	let Some(updated) = edit(current) else {
		return Ok(());
	};
	if updated.is_empty() {
		map.remove("packages");
	} else {
		map.insert("packages".to_string(), Value::Array(updated));
	}
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}
	let body =
		serde_json::to_string_pretty(&Value::Object(map)).map_err(|error| error.to_string())?;
	std::fs::write(path, body + "\n").map_err(|error| error.to_string())
}

// -- per-resource pattern filters (upstream package-manager.ts:677-845) ------

/// The four resource types a settings package entry can filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
	Extensions,
	Skills,
	Prompts,
	Themes,
}

impl ResourceType {
	pub const fn as_str(self) -> &'static str {
		match self {
			ResourceType::Extensions => "extensions",
			ResourceType::Skills => "skills",
			ResourceType::Prompts => "prompts",
			ResourceType::Themes => "themes",
		}
	}

	pub const ALL: [ResourceType; 4] = [
		ResourceType::Extensions,
		ResourceType::Skills,
		ResourceType::Prompts,
		ResourceType::Themes,
	];
}

/// Glob match like upstream `matchesAnyPattern` (minimatch against the
/// base-relative path, the file name, and the full path; SKILL.md files also
/// match their parent directory's rel/name/full forms).
fn matches_any_pattern(file: &Path, patterns: &[String], base_dir: &Path) -> bool {
	let rel = relative_posix(base_dir, file);
	let name = file
		.file_name()
		.map(|name| name.to_string_lossy().to_string())
		.unwrap_or_default();
	let full = file.to_string_lossy().replace('\\', "/");
	let skill_parent = (name == "SKILL.md").then(|| file.parent().map(Path::to_path_buf));
	patterns.iter().any(|pattern| {
		let normalized = pattern.replace('\\', "/");
		let glob = globset::Glob::new(&normalized)
			.ok()
			.map(|glob| glob.compile_matcher());
		let matches = move |candidate: &str| match &glob {
			Some(matcher) => matcher.is_match(candidate),
			// Uncompilable globs degrade to literal equality (minimatch with
			// a literal pattern is an exact match).
			None => candidate == normalized,
		};
		matches(&rel) || matches(&name) || matches(&full) || {
			let Some(Some(parent)) = &skill_parent else {
				return false;
			};
			let parent_rel = relative_posix(base_dir, parent);
			let parent_name = parent
				.file_name()
				.map(|name| name.to_string_lossy().to_string())
				.unwrap_or_default();
			let parent_full = parent.to_string_lossy().replace('\\', "/");
			matches(&parent_rel) || matches(&parent_name) || matches(&parent_full)
		}
	})
}

/// Exact match like upstream `matchesAnyExactPattern` (`./` stripped, the
/// pattern compared against the base-relative and full path; SKILL.md files
/// also match their parent directory's forms).
fn matches_any_exact(file: &Path, patterns: &[String], base_dir: &Path) -> bool {
	if patterns.is_empty() {
		return false;
	}
	let rel = relative_posix(base_dir, file);
	let full = file.to_string_lossy().replace('\\', "/");
	let skill_parent = (rel == "SKILL.md"
		|| file.file_name().is_some_and(|name| name == "SKILL.md"))
	.then(|| file.parent().map(Path::to_path_buf));
	patterns.iter().any(|pattern| {
		let normalized = pattern
			.strip_prefix("./")
			.or_else(|| pattern.strip_prefix(".\\"))
			.unwrap_or(pattern)
			.replace('\\', "/");
		normalized == rel || normalized == full || {
			let Some(Some(parent)) = &skill_parent else {
				return false;
			};
			let parent_rel = relative_posix(base_dir, parent);
			let parent_name = parent
				.file_name()
				.map(|name| name.to_string_lossy().to_string())
				.unwrap_or_default();
			let parent_full = parent.to_string_lossy().replace('\\', "/");
			normalized == parent_rel || normalized == parent_name || normalized == parent_full
		}
	})
}

/// Apply a settings entry's pattern list and return the enabled file set —
/// upstream `applyPatterns`: plain patterns include (glob), `!` excludes
/// (glob), `+` force-includes (exact, beats `!`), `-` force-excludes (exact,
/// beats everything).
pub fn apply_patterns(
	files: &[PathBuf],
	patterns: &[String],
	base_dir: &Path,
) -> std::collections::HashSet<PathBuf> {
	let mut includes = Vec::new();
	let mut excludes = Vec::new();
	let mut force_includes = Vec::new();
	let mut force_excludes = Vec::new();
	for pattern in patterns {
		if let Some(rest) = pattern.strip_prefix('+') {
			force_includes.push(rest.to_string());
		} else if let Some(rest) = pattern.strip_prefix('-') {
			force_excludes.push(rest.to_string());
		} else if let Some(rest) = pattern.strip_prefix('!') {
			excludes.push(rest.to_string());
		} else {
			includes.push(pattern.clone());
		}
	}

	let mut enabled: Vec<&PathBuf> = if includes.is_empty() {
		files.iter().collect()
	} else {
		files
			.iter()
			.filter(|file| matches_any_pattern(file, &includes, base_dir))
			.collect()
	};
	if !excludes.is_empty() {
		enabled.retain(|file| !matches_any_pattern(file, &excludes, base_dir));
	}
	if !force_includes.is_empty() {
		for file in files {
			if !enabled.iter().any(|kept| kept.as_path() == file)
				&& matches_any_exact(file, &force_includes, base_dir)
			{
				enabled.push(file);
			}
		}
	}
	if !force_excludes.is_empty() {
		enabled.retain(|file| !matches_any_exact(file, &force_excludes, base_dir));
	}
	enabled.into_iter().cloned().collect()
}

/// The `packages` entries of one settings file as raw values (string or
/// object), preserving filter arrays and the `autoload` flag.
fn read_entries(path: &Path) -> Vec<Value> {
	let Ok(text) = std::fs::read_to_string(path) else {
		return Vec::new();
	};
	let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
	else {
		return Vec::new();
	};
	match map.get("packages") {
		Some(Value::Array(items)) => items.clone(),
		_ => Vec::new(),
	}
}

/// The pattern list one settings entry carries for a resource type.
fn entry_patterns(entry: &Value, resource_type: ResourceType) -> Vec<String> {
	entry
		.get(resource_type.as_str())
		.and_then(Value::as_array)
		.map(|items| {
			items
				.iter()
				.filter_map(Value::as_str)
				.map(str::to_string)
				.collect()
		})
		.unwrap_or_default()
}

/// One item of the package resource selector: a discovered file, its package,
/// and the enabled/override state the settings produce.
#[derive(Debug, Clone)]
pub struct ResourceItem {
	pub path: PathBuf,
	/// Package-root-relative posix path — the pattern the selector writes.
	pub rel: String,
	pub resource_type: ResourceType,
	/// Enabled per the defining settings entry's pattern list.
	pub enabled: bool,
	/// Override from the other scope's same-identity entry — `None` inherits.
	pub project_override: Option<bool>,
	/// The settings entry that lists the package (`"user"` or `"project"`).
	pub package_scope: &'static str,
	pub package_source: String,
}

impl ResourceItem {
	/// The state a project-mode view renders: override when set, else the
	/// defining entry's enabled.
	pub fn effective(&self) -> bool {
		self.project_override.unwrap_or(self.enabled)
	}
}

/// Project-override state for one resource from the other scope's entry —
/// upstream `getOverrideStateFromEntries`: an empty array means unload-all
/// (unless the entry is an `autoload: false` override shell), `+pattern`
/// loads, `-`/`!` unloads, matched by exact string equality against the
/// file's package-relative path.
pub fn project_override_state(
	entry: &Value,
	resource_type: ResourceType,
	rel_pattern: &str,
) -> Option<bool> {
	// A missing array inherits; an explicit empty array unloads everything
	// (unless the entry is an `autoload: false` override shell).
	let patterns = entry.get(resource_type.as_str())?.as_array()?;
	if patterns.is_empty() {
		return (entry.get("autoload").and_then(Value::as_bool) != Some(false)).then_some(false);
	}
	let normalize = |pattern: &str| {
		pattern
			.strip_prefix("./")
			.or_else(|| pattern.strip_prefix(".\\"))
			.unwrap_or(pattern)
			.replace('\\', "/")
	};
	let target = normalize(rel_pattern);
	let mut state = None;
	for pattern in patterns {
		let Some(pattern) = pattern.as_str() else {
			continue;
		};
		let stripped = pattern
			.strip_prefix('+')
			.or_else(|| pattern.strip_prefix('-'))
			.or_else(|| pattern.strip_prefix('!'))
			.unwrap_or(pattern);
		if normalize(stripped) != target {
			continue;
		}
		state = Some(!pattern.starts_with('-') && !pattern.starts_with('!'));
	}
	state
}

/// The package manager: install/remove/update/list plus resource discovery —
/// upstream `DefaultPackageManager` (reduced).
pub struct PackageManager {
	pub cwd: PathBuf,
	pub agent_dir: PathBuf,
	pub project_trusted: bool,
}

impl PackageManager {
	pub fn new(cwd: &Path, agent_dir: &Path, project_trusted: bool) -> Self {
		Self {
			cwd: cwd.to_path_buf(),
			agent_dir: agent_dir.to_path_buf(),
			project_trusted,
		}
	}

	fn settings_paths(&self) -> (PathBuf, Option<PathBuf>) {
		(
			crate::config::settings_json_path(),
			Some(crate::config::project_pi_dir(&self.cwd).join("settings.json")),
		)
	}

	fn npm_root(&self, local: bool) -> Result<PathBuf, String> {
		if local {
			self.assert_project_trusted()?;
			Ok(self.cwd.join(".pi").join("npm"))
		} else {
			Ok(self.agent_dir.join("npm"))
		}
	}

	fn git_root(&self, local: bool) -> Result<PathBuf, String> {
		if local {
			self.assert_project_trusted()?;
			Ok(self.cwd.join(".pi").join("git"))
		} else {
			Ok(self.agent_dir.join("git"))
		}
	}

	fn assert_project_trusted(&self) -> Result<(), String> {
		if self.project_trusted {
			Ok(())
		} else {
			Err("Project is not trusted; refusing to access project package storage".to_string())
		}
	}

	/// `<npm root>/node_modules/<name>` for a parsed npm source.
	fn npm_install_path(&self, name: &str, local: bool) -> Result<PathBuf, String> {
		let mut path = self.npm_root(local)?.join("node_modules");
		for segment in name.split('/') {
			path.push(segment);
		}
		Ok(path)
	}

	/// `<git root>/<host>/<path>` for a parsed git source.
	fn git_install_path(&self, host: &str, path: &str, local: bool) -> Result<PathBuf, String> {
		let mut install = self.git_root(local)?;
		install.push(host);
		for segment in path.split('/') {
			install.push(segment);
		}
		Ok(install)
	}

	/// Install path for a source string, `None` when nothing is installed
	/// there (local sources resolve in place) — upstream `getInstalledPath`.
	pub fn installed_path(&self, source: &str, local: bool) -> Option<PathBuf> {
		match parse_source(source) {
			ParsedSource::Npm { name, .. } => {
				let path = self.npm_install_path(&name, local).ok()?;
				path.exists().then_some(path)
			}
			ParsedSource::Git { host, path, .. } => {
				let path = self.git_install_path(&host, &path, local).ok()?;
				path.exists().then_some(path)
			}
			ParsedSource::Local { path } => {
				let path = resolve_path(&self.cwd, &path);
				path.exists().then_some(path)
			}
		}
	}

	/// `install <source>` — npm install / git clone / local existence check
	/// (upstream `install`).
	pub async fn install(&self, source: &str, local: bool) -> Result<(), String> {
		self.install_parsed(&parse_source(source), local).await
	}

	/// [`Self::install`] over an already-parsed source — test-visible so the
	/// clone mechanics can run offline against a local path (the URL grammar
	/// only reaches git for network hosts, like upstream).
	pub(crate) async fn install_parsed(
		&self,
		parsed: &ParsedSource,
		local: bool,
	) -> Result<(), String> {
		match parsed {
			ParsedSource::Npm { spec, .. } => {
				let root = self.npm_root(local)?;
				ensure_npm_project(&root)?;
				let root_string = root.to_string_lossy().into_owned();
				let (command, mut args) = {
					let settings = read_settings_json(&self.settings_paths().0);
					let (command, args) = npm_command(&settings);
					(command, args)
				};
				args.push("install".to_string());
				args.push(spec.clone());
				args.push("--prefix".to_string());
				args.push(root_string);
				args.push("--legacy-peer-deps".to_string());
				run_command(&command, &args, None, INSTALL_TIMEOUT).await
			}
			ParsedSource::Git {
				repo,
				host,
				path,
				ref_,
				..
			} => {
				let target = self.git_install_path(host, path, local)?;
				if target.exists() {
					// Already cloned: reconcile the configured ref (upstream
					// ensureGitRef through installGit's existsSync branch).
					return self.ensure_git_ref(&target, ref_);
				}
				let root = self.git_root(local)?;
				let _ = std::fs::create_dir_all(&root);
				let _ = ensure_git_ignore(&root);
				if let Some(parent) = target.parent() {
					let _ = std::fs::create_dir_all(parent);
				}
				run_command(
					"git",
					&[
						"clone".to_string(),
						repo.clone(),
						target.to_string_lossy().into_owned(),
					],
					None,
					INSTALL_TIMEOUT,
				)
				.await
				.inspect_err(|_| {
					let _ = std::fs::remove_dir_all(&target);
				})?;
				if let Some(git_ref) = &ref_ {
					run_command(
						"git",
						&["checkout".to_string(), git_ref.clone()],
						Some(&target),
						NETWORK_TIMEOUT,
					)
					.await?;
				}
				self.install_git_dependencies(&target).await
			}
			ParsedSource::Local { path } => {
				let resolved = resolve_path(&self.cwd, path);
				if resolved.exists() {
					Ok(())
				} else {
					Err(format!("Path does not exist: {}", resolved.display()))
				}
			}
		}
	}

	/// git clone with a configured ref: fetch it and check the clone sits on
	/// it (upstream `installGit`'s exists branch via `ensureGitRef`).
	fn ensure_git_ref(&self, target: &Path, git_ref: &Option<String>) -> Result<(), String> {
		let _ = target;
		let _ = git_ref;
		// The reduced port keeps an existing clone as-is; `update` performs
		// the fetch/reset reconciliation.
		Ok(())
	}

	/// `npm install --omit=dev --legacy-peer-deps` inside a git checkout
	/// (upstream `cleanAndInstallGitDependencies`'s npm step).
	async fn install_git_dependencies(&self, target: &Path) -> Result<(), String> {
		if !target.join("package.json").exists() {
			return Ok(());
		}
		let (command, mut args) = {
			let settings = read_settings_json(&self.settings_paths().0);
			let (command, args) = npm_command(&settings);
			(command, args)
		};
		args.push("install".to_string());
		args.push("--omit=dev".to_string());
		args.push("--legacy-peer-deps".to_string());
		run_command(&command, &args, Some(target), INSTALL_TIMEOUT).await
	}

	/// `remove <source>` — uninstall the install artifacts; the settings
	/// entry cleanup is the caller's (`remove_and_persist`). Returns whether
	/// anything was removed (upstream `remove` + `removeSourceFromSettings`).
	pub async fn remove(&self, source: &str, local: bool) -> Result<(), String> {
		self.remove_parsed(&parse_source(source), local).await
	}

	/// [`Self::remove`] over an already-parsed source (test-visible like
	/// [`Self::install_parsed`]).
	pub(crate) async fn remove_parsed(
		&self,
		parsed: &ParsedSource,
		local: bool,
	) -> Result<(), String> {
		match parsed {
			ParsedSource::Npm { name, .. } => {
				let path = self.npm_install_path(name, local)?;
				if !path.exists() {
					return Ok(());
				}
				let root = self.npm_root(local)?;
				let (command, mut args) = {
					let settings = read_settings_json(&self.settings_paths().0);
					let (command, args) = npm_command(&settings);
					(command, args)
				};
				args.push("uninstall".to_string());
				args.push(name.clone());
				args.push("--prefix".to_string());
				args.push(root.to_string_lossy().into_owned());
				run_command(&command, &args, None, INSTALL_TIMEOUT).await
			}
			ParsedSource::Git { host, path, .. } => {
				let target = self.git_install_path(host, path, local)?;
				if target.exists() {
					std::fs::remove_dir_all(&target).map_err(|error| {
						format!("failed to remove {}: {error}", target.display())
					})?;
				}
				prune_empty_git_parents(&target, &self.git_root(local)?);
				Ok(())
			}
			ParsedSource::Local { .. } => Ok(()),
		}
	}

	/// `update [source]` — refresh configured npm (unpinned) and git sources;
	/// `source` filters by identity and errors when nothing matches (upstream
	/// `update`).
	pub async fn update(&self, source: Option<&str>) -> Result<(), String> {
		if is_offline() {
			return Ok(());
		}
		let (global_path, project_path) = self.settings_paths();
		let mut updates: Vec<(String, bool)> = Vec::new();
		if let Some(project) = &project_path {
			for entry in read_sources(project) {
				updates.push((entry, true));
			}
		}
		for entry in read_sources(&global_path) {
			updates.push((entry, false));
		}
		if let Some(source) = source {
			let identity = parse_source(source).identity(Some(&self.cwd));
			if !updates
				.iter()
				.any(|(entry, _)| parse_source(entry).identity(Some(&self.cwd)) == identity)
			{
				return Err(format!("No matching package found for {source}"));
			}
			updates.retain(|(entry, _)| parse_source(entry).identity(Some(&self.cwd)) == identity);
		}
		for (entry, local) in updates {
			match parse_source(&entry) {
				ParsedSource::Npm {
					spec, name, pinned, ..
				} if !pinned => {
					let installed = self.npm_install_path(&name, local)?;
					if !installed.exists() {
						continue;
					}
					if self.npm_has_update(&spec, &name, &installed).await {
						self.install(&entry, local).await?;
					}
				}
				ParsedSource::Git {
					repo,
					host,
					path,
					ref_,
					..
				} => {
					let target = self.git_install_path(&host, &path, local)?;
					if !target.exists() {
						continue;
					}
					self.update_git(&target, &repo, &ref_).await?;
				}
				_ => {}
			}
		}
		Ok(())
	}

	/// `npm view <spec> version` vs the installed version (upstream
	/// `npmHasAvailableUpdate`, reduced to the plain latest check).
	async fn npm_has_update(&self, spec: &str, _name: &str, installed: &Path) -> bool {
		let Ok(installed_version) = std::fs::read_to_string(installed.join("package.json")) else {
			return false;
		};
		let Ok(Value::Object(map)) =
			serde_json::from_str::<Value>(installed_version.trim_start_matches('\u{feff}'))
		else {
			return false;
		};
		let Some(current) = map.get("version").and_then(Value::as_str) else {
			return false;
		};
		let Ok(current) = semver::Version::parse(current) else {
			return false;
		};
		let (command, mut args) = {
			let settings = read_settings_json(&self.settings_paths().0);
			let (command, args) = npm_command(&settings);
			(command, args)
		};
		args.push("view".to_string());
		args.push(spec.to_string());
		args.push("version".to_string());
		let Ok(latest) = run_command_capture(&command, &args, None, NETWORK_TIMEOUT).await else {
			return false;
		};
		match semver::Version::parse(latest.trim()) {
			Ok(latest) => latest > current,
			Err(_) => false,
		}
	}

	/// Git update: fetch the configured ref (or the branch's upstream), and
	/// `reset --hard` when the clone is behind — upstream `ensureGitRef`
	/// reduced (no update-marker clean-reinstall ladder).
	async fn update_git(
		&self,
		target: &Path,
		_repo: &str,
		git_ref: &Option<String>,
	) -> Result<(), String> {
		let (fetch_args, commit_ref) = match git_ref {
			Some(git_ref) => (
				vec!["fetch".to_string(), "origin".to_string(), git_ref.clone()],
				"FETCH_HEAD".to_string(),
			),
			None => {
				// The clone's upstream branch (upstream
				// `getLocalGitUpdateTarget`).
				let upstream = run_command_capture(
					"git",
					&[
						"rev-parse".to_string(),
						"--abbrev-ref".to_string(),
						"@{upstream}".to_string(),
					],
					Some(target),
					NETWORK_TIMEOUT,
				)
				.await?;
				let upstream = upstream.trim().to_string();
				let Some(branch) = upstream.strip_prefix("origin/") else {
					return Ok(());
				};
				(
					vec![
						"fetch".to_string(),
						"origin".to_string(),
						branch.to_string(),
					],
					format!("origin/{branch}"),
				)
			}
		};
		run_command("git", &fetch_args, Some(target), NETWORK_TIMEOUT).await?;
		let local_head = run_command_capture(
			"git",
			&["rev-parse".to_string(), "HEAD".to_string()],
			Some(target),
			NETWORK_TIMEOUT,
		)
		.await?;
		let target_head = run_command_capture(
			"git",
			&["rev-parse".to_string(), format!("{commit_ref}^{{commit}}")],
			Some(target),
			NETWORK_TIMEOUT,
		)
		.await?;
		if local_head.trim() == target_head.trim() {
			return Ok(());
		}
		run_command(
			"git",
			&["reset".to_string(), "--hard".to_string(), commit_ref],
			Some(target),
			NETWORK_TIMEOUT,
		)
		.await?;
		self.install_git_dependencies(target).await
	}

	/// Persist `packages` into the global (or project with `local`) settings
	/// file — upstream `addSourceToSettings` (dedupe by identity).
	pub fn add_source_to_settings(&self, source: &str, local: bool) -> Result<bool, String> {
		let identity = parse_source(source).identity(Some(&self.cwd));
		let path = if local {
			self.assert_project_trusted()?;
			self.settings_paths().1.expect("project path with cwd")
		} else {
			self.settings_paths().0
		};
		edit_packages(&path, |mut packages| {
			let already = packages.iter().any(|entry| {
				entry
					.as_str()
					.is_some_and(|entry| parse_source(entry).identity(Some(&self.cwd)) == identity)
			});
			if already {
				return None;
			}
			packages.push(Value::String(source.to_string()));
			Some(packages)
		})?;
		Ok(true)
	}

	/// Remove the matching settings entry; `Ok(false)` when no entry matched
	/// (upstream `removeSourceFromSettings`).
	pub fn remove_source_from_settings(&self, source: &str, local: bool) -> Result<bool, String> {
		let identity = parse_source(source).identity(Some(&self.cwd));
		let path = if local {
			self.assert_project_trusted()?;
			self.settings_paths().1.expect("project path with cwd")
		} else {
			self.settings_paths().0
		};
		let mut removed = false;
		edit_packages(&path, |packages| {
			let mut kept = Vec::new();
			for entry in packages {
				let matches = entry
					.as_str()
					.is_some_and(|entry| parse_source(entry).identity(Some(&self.cwd)) == identity);
				if matches {
					removed = true;
				} else {
					kept.push(entry);
				}
			}
			if !removed {
				return None;
			}
			Some(kept)
		})?;
		Ok(removed)
	}

	/// The configured packages across both scopes — upstream
	/// `listConfiguredPackages` (project first, identity dedupe with project
	/// winning).
	pub fn list_configured(&self) -> Vec<ConfiguredPackage> {
		let (global_path, project_path) = self.settings_paths();
		let mut seen = Vec::new();
		let mut packages = Vec::new();
		let push = |entry: Value,
		            local: bool,
		            packages: &mut Vec<ConfiguredPackage>,
		            seen: &mut Vec<String>| {
			let Some(source) = entry.as_str().map(str::to_string).or_else(|| {
				entry
					.get("source")
					.and_then(Value::as_str)
					.map(str::to_string)
			}) else {
				return;
			};
			let identity = parse_source(&source).identity(Some(&self.cwd));
			if seen.contains(&identity) {
				return;
			}
			seen.push(identity);
			let installed_path = self.installed_path(&source, local);
			packages.push(ConfiguredPackage {
				source,
				scope: if local { "project" } else { "user" },
				installed_path,
				entry,
			});
		};
		if let Some(project) = &project_path {
			for entry in read_entries(project) {
				push(entry, true, &mut packages, &mut seen);
			}
		}
		for entry in read_entries(&global_path) {
			push(entry, false, &mut packages, &mut seen);
		}
		packages
	}

	/// Discover resources from every installed package — upstream
	/// `resolve()`'s package branch (missing installs become warnings; the
	/// interactive auto-install prompt is a declared reduction). Per-resource
	/// pattern filters in the settings entries decide which discovered files
	/// stay enabled: the defining entry's patterns are the base, and a
	/// project-scope entry for the same package overrides item by item
	/// (upstream `getOverrideStateFromEntries`).
	pub fn resolve_resources(&self) -> PackageResources {
		let (items, warnings) = self.resolve_items();
		let mut resources = PackageResources {
			warnings,
			..Default::default()
		};
		for item in items {
			if !item.effective() {
				continue;
			}
			match item.resource_type {
				ResourceType::Extensions => resources.extensions.push(item.path),
				ResourceType::Skills => resources.skills.push(item.path),
				ResourceType::Prompts => resources.prompts.push(item.path),
				ResourceType::Themes => resources.themes.push(item.path),
			}
		}
		resources
	}

	/// The selector-facing resolve: every discovered file with its per-scope
	/// settings state (see [`ResourceItem`]). Both settings files are walked
	/// independently — the global entry defines loading (a project-only
	/// package defines its own), and a same-identity project entry overrides
	/// item by item.
	pub fn resolve_items(&self) -> (Vec<ResourceItem>, Vec<String>) {
		let (global_path, project_path) = self.settings_paths();
		let project_entries = project_path
			.as_ref()
			.map(|path| read_entries(path))
			.unwrap_or_default();
		let mut items = Vec::new();
		let mut warnings = Vec::new();
		let mut seen: Vec<String> = Vec::new();

		let mut resolve_one =
			|source: &str, user_entry: Option<&Value>, project_entry: Option<&Value>| {
				let identity = parse_source(source).identity(Some(&self.cwd));
				if seen.contains(&identity) {
					return;
				}
				seen.push(identity);
				let installed = self
					.installed_path(source, project_entry.is_some())
					.or_else(|| self.installed_path(source, false));
				let Some(installed) = installed else {
					warnings.push(format!(
						"package \"{source}\" is configured but not installed; run: pi-rs package install {source}"
					));
					return;
				};
				let files = collect_package_files(&installed);
				let defining = user_entry.or(project_entry).expect("one entry exists");
				let package_scope = if user_entry.is_some() {
					"user"
				} else {
					"project"
				};
				for resource_type in ResourceType::ALL {
					let type_files = files.for_type(resource_type);
					let base_enabled = apply_patterns(
						type_files,
						&entry_patterns(defining, resource_type),
						&installed,
					);
					for path in type_files {
						let enabled = base_enabled.contains(path.as_path());
						let rel = relative_posix(&installed, path);
						let project_override = project_entry
							.and_then(|entry| project_override_state(entry, resource_type, &rel));
						items.push(ResourceItem {
							path: path.clone(),
							rel,
							resource_type,
							enabled,
							project_override,
							package_scope,
							package_source: source.to_string(),
						});
					}
				}
			};

		// Project-first identity walk (mirrors list_configured's order), but
		// both entries are kept when the same package is listed twice.
		let entry_source = |entry: &Value| -> Option<String> {
			entry.as_str().map(str::to_string).or_else(|| {
				entry
					.get("source")
					.and_then(Value::as_str)
					.map(str::to_string)
			})
		};
		for project_entry in &project_entries {
			let Some(source) = entry_source(project_entry) else {
				continue;
			};
			let user_entry = read_entries(&global_path).into_iter().find(|candidate| {
				entry_source(candidate).is_some_and(|candidate| {
					parse_source(&candidate).identity(Some(&self.cwd))
						== parse_source(&source).identity(Some(&self.cwd))
				})
			});
			resolve_one(&source, user_entry.as_ref(), Some(project_entry));
		}
		for user_entry in read_entries(&global_path) {
			let Some(source) = entry_source(&user_entry) else {
				continue;
			};
			resolve_one(&source, Some(&user_entry), None);
		}
		(items, warnings)
	}

	/// The pattern list a settings file's entry for `source` carries for a
	/// resource type (empty when the entry or the array is absent).
	pub fn package_type_patterns(
		&self,
		source: &str,
		resource_type: ResourceType,
		local: bool,
	) -> Vec<String> {
		let identity = parse_source(source).identity(Some(&self.cwd));
		let path = if local {
			match self.settings_paths().1 {
				Some(path) => path,
				None => return Vec::new(),
			}
		} else {
			self.settings_paths().0
		};
		read_entries(&path)
			.into_iter()
			.find(|entry| {
				entry
					.as_str()
					.map(str::to_string)
					.or_else(|| {
						entry
							.get("source")
							.and_then(Value::as_str)
							.map(str::to_string)
					})
					.is_some_and(|candidate| {
						parse_source(&candidate).identity(Some(&self.cwd)) == identity
					})
			})
			.map(|entry| entry_patterns(&entry, resource_type))
			.unwrap_or_default()
	}

	/// Read-modify-write one settings file's package entry pattern array —
	/// upstream `togglePackageResource`/`setProjectPackageOverride` persistence:
	/// find the entry by identity (promoting string entries to objects), set
	/// (`Some`) or clear (`None`) the type's array, then collapse objects with
	/// no filter arrays left back to plain sources — removing override-only
	/// entries (`autoload: false`) entirely, like upstream.
	pub fn set_package_patterns(
		&self,
		source: &str,
		resource_type: ResourceType,
		patterns: Option<&[String]>,
		local: bool,
	) -> Result<(), String> {
		let identity = parse_source(source).identity(Some(&self.cwd));
		let path = if local {
			self.assert_project_trusted()?;
			self.settings_paths().1.expect("project path with cwd")
		} else {
			self.settings_paths().0
		};
		edit_packages(&path, |mut packages| {
			let index = packages.iter().position(|entry| {
				entry
					.as_str()
					.map(str::to_string)
					.or_else(|| {
						entry
							.get("source")
							.and_then(Value::as_str)
							.map(str::to_string)
					})
					.is_some_and(|candidate| {
						parse_source(&candidate).identity(Some(&self.cwd)) == identity
					})
			});
			let Some(index) = index else {
				// Selector-created override entries start as autoload:false
				// objects (upstream `createPackageOverrideSource`).
				let list = patterns.filter(|list| !list.is_empty())?;
				packages.push(serde_json::json!({
					"source": source,
					"autoload": false,
					resource_type.as_str(): list,
				}));
				return Some(packages);
			};
			let entry = packages[index].clone();
			let mut object = match entry {
				Value::Object(object) => object,
				Value::String(source) => {
					let mut object = serde_json::Map::new();
					object.insert("source".to_string(), Value::String(source));
					object
				}
				_ => return None,
			};
			match patterns {
				Some(list) if !list.is_empty() => {
					object.insert(
						resource_type.as_str().to_string(),
						Value::Array(list.iter().map(|p| Value::String(p.clone())).collect()),
					);
				}
				_ => {
					object.remove(resource_type.as_str());
				}
			}
			let has_filters = ResourceType::ALL
				.iter()
				.any(|ty| object.contains_key(ty.as_str()));
			if has_filters {
				packages[index] = Value::Object(object);
			} else if object.get("autoload").and_then(Value::as_bool) == Some(false) {
				packages.remove(index);
			} else {
				packages[index] = object
					.get("source")
					.cloned()
					.unwrap_or(Value::String(source.to_string()));
			}
			Some(packages)
		})
	}
}

fn read_settings_json(path: &Path) -> serde_json::Map<String, Value> {
	let Ok(text) = std::fs::read_to_string(path) else {
		return Default::default();
	};
	match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
		Ok(Value::Object(map)) => map,
		_ => Default::default(),
	}
}

/// `<root>/package.json` = `{ name: "pi-extensions", private: true }` —
/// upstream `ensureNpmProject` (the cloud-sync marker is a no-op here).
fn ensure_npm_project(root: &Path) -> Result<(), String> {
	std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
	let _ = ensure_git_ignore(root);
	let package_json = root.join("package.json");
	if !package_json.exists() {
		std::fs::write(
			&package_json,
			serde_json::to_string_pretty(&json!({
				"name": "pi-extensions",
				"private": true,
			}))
			.map_err(|error| error.to_string())?,
		)
		.map_err(|error| error.to_string())?;
	}
	Ok(())
}

/// `.gitignore` = `*` — upstream `ensureGitIgnore` (the npm/git stores stay
/// out of version control).
fn ensure_git_ignore(dir: &Path) -> std::io::Result<()> {
	std::fs::create_dir_all(dir)?;
	let ignore_path = dir.join(".gitignore");
	if !ignore_path.exists() {
		std::fs::write(&ignore_path, "*\n")?;
	}
	Ok(())
}

/// Remove now-empty `<git root>/<host>/…` parents (upstream
/// `pruneEmptyGitParents`).
fn prune_empty_git_parents(target: &Path, root: &Path) {
	let mut current = target.parent();
	while let Some(dir) = current {
		if dir == root || !dir.starts_with(root) {
			break;
		}
		if std::fs::remove_dir(dir).is_err() {
			break;
		}
		current = dir.parent();
	}
}

// -------------------------------------------------------------------------------------------
// Package resource discovery — upstream `collectResourceFiles` and the
// extension entry resolution (manifest `pi.extensions` → index files → walk).
// -------------------------------------------------------------------------------------------

/// A package's `pi` manifest (upstream `readPiManifest`).
fn read_pi_manifest(package_root: &Path) -> Option<Value> {
	let manifest: Value =
		serde_json::from_str(&std::fs::read_to_string(package_root.join("package.json")).ok()?)
			.ok()?;
	manifest.get("pi").cloned()
}

/// Manifest-declared or index-file extension entries of one directory —
/// upstream `resolveExtensionEntries`.
fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
	if let Some(Value::Array(entries)) =
		read_pi_manifest(dir).and_then(|manifest| manifest.get("extensions").cloned())
	{
		let resolved: Vec<PathBuf> = entries
			.iter()
			.filter_map(Value::as_str)
			.map(|entry| dir.join(entry))
			.filter(|entry| entry.exists())
			.collect();
		if !resolved.is_empty() {
			return Some(resolved);
		}
	}
	for name in ["index.ts", "index.js"] {
		let index = dir.join(name);
		if index.exists() {
			return Some(vec![index]);
		}
	}
	None
}

/// Recursive extension discovery — upstream `collectAutoExtensionEntries`:
/// the root's explicit entries win, else every *.ts/*.js file is an entry and
/// every subdirectory with explicit entries contributes those.
fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
	let mut entries = Vec::new();
	if !dir.exists() {
		return entries;
	}
	if let Some(root_entries) = resolve_extension_entries(dir) {
		return root_entries;
	}
	let Ok(read) = std::fs::read_dir(dir) else {
		return entries;
	};
	let mut rules = IgnoreRules::new();
	rules.add_dir_rules(dir, dir);
	let mut children: Vec<_> = read
		.flatten()
		.filter(|entry| {
			let name = entry.file_name();
			let name = name.to_string_lossy();
			!name.starts_with('.') && name != "node_modules"
		})
		.map(|entry| entry.path())
		.collect();
	children.sort();
	for child in children {
		let is_dir = child.is_dir();
		let rel = relative_posix(dir, &child);
		if rules.ignores(&rel, is_dir) {
			continue;
		}
		if child.is_file()
			&& child
				.extension()
				.is_some_and(|ext| ext == "ts" || ext == "js")
		{
			entries.push(child);
		} else if is_dir && let Some(resolved) = resolve_extension_entries(&child) {
			entries.extend(resolved);
		}
	}
	entries
}

/// `SKILL.md` files nearest-first per directory — upstream
/// `collectSkillEntries(dir, "pi")`.
fn collect_skill_entries(dir: &Path) -> Vec<PathBuf> {
	let mut entries = Vec::new();
	collect_skill_entries_inner(dir, dir, &mut IgnoreRules::new(), &mut entries);
	entries
}

fn collect_skill_entries_inner(
	dir: &Path,
	root: &Path,
	rules: &mut IgnoreRules,
	entries: &mut Vec<PathBuf>,
) {
	if !dir.exists() {
		return;
	}
	rules.add_dir_rules(dir, root);
	let Ok(read) = std::fs::read_dir(dir) else {
		return;
	};
	let skill = dir.join("SKILL.md");
	if skill.is_file() {
		let rel = relative_posix(root, &skill);
		if !rules.ignores(&rel, false) {
			entries.push(skill);
			// One skill per directory (upstream returns after the first
			// SKILL.md).
			return;
		}
	}
	let mut children: Vec<_> = read
		.flatten()
		.filter(|entry| {
			let name = entry.file_name();
			let name = name.to_string_lossy();
			!name.starts_with('.') && name != "node_modules" && name != "SKILL.md"
		})
		.map(|entry| entry.path())
		.collect();
	children.sort();
	for child in children {
		if child.is_dir() {
			collect_skill_entries_inner(&child, root, rules, entries);
		}
	}
}

/// Recursive `*.md`/`*.json` collection for prompts/themes — upstream
/// `collectFiles` with the type pattern.
fn collect_pattern_files(dir: &Path, extension: &str) -> Vec<PathBuf> {
	let mut files = Vec::new();
	collect_pattern_files_inner(dir, dir, &mut IgnoreRules::new(), extension, &mut files);
	files
}

fn collect_pattern_files_inner(
	dir: &Path,
	root: &Path,
	rules: &mut IgnoreRules,
	extension: &str,
	files: &mut Vec<PathBuf>,
) {
	if !dir.exists() {
		return;
	}
	rules.add_dir_rules(dir, root);
	let Ok(read) = std::fs::read_dir(dir) else {
		return;
	};
	let mut children: Vec<_> = read
		.flatten()
		.filter(|entry| {
			let name = entry.file_name();
			let name = name.to_string_lossy();
			!name.starts_with('.') && name != "node_modules"
		})
		.map(|entry| entry.path())
		.collect();
	children.sort();
	for child in children {
		let is_dir = child.is_dir();
		let rel = relative_posix(root, &child);
		if rules.ignores(&rel, is_dir) {
			continue;
		}
		if is_dir {
			collect_pattern_files_inner(&child, root, rules, extension, files);
		} else if child.extension().is_some_and(|ext| ext == extension) {
			files.push(child);
		}
	}
}

/// Discover every resource type under one installed package root — upstream
/// `collectDefaultResources`: the manifest's `pi.<type>` entries win, else
/// the `<root>/<type>/` convention directory is walked.
/// Per-type discovered files inside one package root — the raw material the
/// pattern filters select from.
#[derive(Debug, Default, Clone)]
struct PackageFiles {
	extensions: Vec<PathBuf>,
	skills: Vec<PathBuf>,
	prompts: Vec<PathBuf>,
	themes: Vec<PathBuf>,
}

impl PackageFiles {
	fn for_type(&self, resource_type: ResourceType) -> &Vec<PathBuf> {
		match resource_type {
			ResourceType::Extensions => &self.extensions,
			ResourceType::Skills => &self.skills,
			ResourceType::Prompts => &self.prompts,
			ResourceType::Themes => &self.themes,
		}
	}
}

fn collect_package_files(package_root: &Path) -> PackageFiles {
	let mut files = PackageFiles::default();
	let manifest = read_pi_manifest(package_root);
	for (resource_type, extension, sink) in [
		("extensions", None, 0usize),
		("skills", None, 1),
		("prompts", Some("md"), 2),
		("themes", Some("json"), 3),
	] {
		// Manifest entries first (existing paths only; glob/negation
		// patterns in manifests are a declared reduction).
		if let Some(Value::Array(entries)) = manifest
			.as_ref()
			.and_then(|manifest| manifest.get(resource_type))
		{
			let resolved: Vec<PathBuf> = entries
				.iter()
				.filter_map(Value::as_str)
				.map(|entry| package_root.join(entry))
				.filter(|entry| entry.exists())
				.collect();
			if !resolved.is_empty() {
				match sink {
					0 => files.extensions.extend(resolved),
					1 => files.skills.extend(resolved),
					2 => files.prompts.extend(resolved),
					_ => files.themes.extend(resolved),
				}
				continue;
			}
		}
		let dir = package_root.join(resource_type);
		if !dir.exists() {
			continue;
		}
		let discovered = match extension {
			None => {
				if resource_type == "extensions" {
					collect_auto_extension_entries(&dir)
				} else {
					collect_skill_entries(&dir)
				}
			}
			Some(extension) => collect_pattern_files(&dir, extension),
		};
		match sink {
			0 => files.extensions.extend(discovered),
			1 => files.skills.extend(discovered),
			2 => files.prompts.extend(discovered),
			_ => files.themes.extend(discovered),
		}
	}
	files
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Local-path package round trip: the source stays in place (upstream
	/// local sources are existence-checked, not copied), settings persist,
	/// and resources resolve from the source directory. Registry npm
	/// installs need a registry and are exercised through the CLI instead.
	#[tokio::test(flavor = "multi_thread")]
	async fn local_package_round_trips_through_settings_and_resources() {
		let _guard = ENV_LOCK.lock().await;
		let root = std::env::temp_dir().join(format!(
			"pi-pm-e2e-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(root.join("agent")).unwrap();
		// The settings paths derive from the agent dir — isolate per test
		// (env-var races with other agent-dir tests are avoided by not
		// touching the real home).
		let previous = std::env::var_os(crate::config::AGENT_DIR_ENV);
		unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, root.join("agent")) };
		let fixture = root.join("fixture-pkg");
		std::fs::create_dir_all(fixture.join("extensions")).unwrap();
		std::fs::write(
			fixture.join("extensions/ext.ts"),
			"export default () => {};",
		)
		.unwrap();

		let agent_dir = root.join("agent");
		std::fs::create_dir_all(&agent_dir).unwrap();
		let manager = PackageManager::new(&root, &agent_dir, false);

		let source = fixture.to_string_lossy().into_owned();
		manager.install(&source, false).await.unwrap();
		manager.add_source_to_settings(&source, false).unwrap();
		let configured = manager.list_configured();
		assert_eq!(configured.len(), 1);
		assert_eq!(configured[0].scope, "user");
		assert_eq!(
			configured[0].installed_path.as_deref(),
			Some(fixture.as_path())
		);
		let resources = manager.resolve_resources();
		assert_eq!(
			resources.extensions,
			vec![fixture.join("extensions/ext.ts")]
		);
		assert!(resources.warnings.is_empty(), "{:?}", resources.warnings);

		assert!(manager.remove_source_from_settings(&source, false).unwrap());
		assert!(manager.list_configured().is_empty());

		match previous {
			Some(value) => unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, value) },
			None => unsafe { std::env::remove_var(crate::config::AGENT_DIR_ENV) },
		}
		let _ = std::fs::remove_dir_all(&root);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn git_clone_round_trip_with_a_local_repo() {
		if tokio::process::Command::new("git")
			.arg("--version")
			.output()
			.await
			.is_err()
		{
			eprintln!("skipping: git not available");
			return;
		}
		let _guard = ENV_LOCK.lock().await;
		let root = std::env::temp_dir().join(format!(
			"pi-pm-git-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let repo = root.join("repo");
		std::fs::create_dir_all(&repo).unwrap();
		let run = |args: &[&str], cwd: Option<&std::path::Path>| {
			let mut cmd = std::process::Command::new("git");
			cmd.args(args);
			if let Some(cwd) = cwd {
				cmd.current_dir(cwd);
			}
			cmd.env("GIT_AUTHOR_NAME", "t")
				.env("GIT_AUTHOR_EMAIL", "t@t")
				.env("GIT_COMMITTER_NAME", "t")
				.env("GIT_COMMITTER_EMAIL", "t@t");
			assert!(cmd.output().unwrap().status.success(), "git {args:?}");
		};
		run(&["init", "-q"], Some(&repo));
		std::fs::create_dir_all(repo.join("extensions")).unwrap();
		std::fs::write(repo.join("extensions/index.ts"), "export default () => {};").unwrap();
		run(&["add", "."], Some(&repo));
		run(&["commit", "-qm", "init"], Some(&repo));

		let agent_dir = root.join("agent");
		std::fs::create_dir_all(&agent_dir).unwrap();
		let previous = std::env::var_os(crate::config::AGENT_DIR_ENV);
		unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, &agent_dir) };
		let manager = PackageManager::new(&root, &agent_dir, false);
		let repo_path = repo.to_string_lossy().into_owned();
		// The URL grammar only reaches git for network hosts (like
		// upstream), so the offline test drives the parsed-source internals
		// with a local path as the clone URL.
		let parsed = ParsedSource::Git {
			repo: repo_path.clone(),
			host: "local.test".to_string(),
			path: "repo".to_string(),
			ref_: None,
			pinned: false,
		};
		manager.install_parsed(&parsed, false).await.unwrap();
		let cloned = agent_dir.join("git/local.test/repo");
		assert!(cloned.join("extensions/index.ts").exists());

		// Update: the clone sits on the upstream head — a no-op reset path.
		manager
			.update_git(&cloned, &repo_path, &None)
			.await
			.unwrap();
		assert!(cloned.join("extensions/index.ts").exists());

		manager.remove_parsed(&parsed, false).await.unwrap();
		assert!(!cloned.exists());
		match previous {
			Some(value) => unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, value) },
			None => unsafe { std::env::remove_var(crate::config::AGENT_DIR_ENV) },
		}
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn npm_specs_parse_into_name_version_range_and_pin() {
		match parse_source("npm:lodash") {
			ParsedSource::Npm {
				spec,
				name,
				version,
				range,
				pinned,
			} => {
				assert_eq!(spec, "lodash");
				assert_eq!(name, "lodash");
				assert_eq!(version, None);
				assert_eq!(range, None);
				assert!(!pinned);
			}
			other => panic!("{other:?}"),
		}
		match parse_source("npm:@scope/pkg@1.2.3") {
			ParsedSource::Npm {
				name,
				version,
				pinned,
				..
			} => {
				assert_eq!(name, "@scope/pkg");
				assert_eq!(version.as_deref(), Some("1.2.3"));
				assert!(pinned, "exact versions pin");
			}
			other => panic!("{other:?}"),
		}
		match parse_source("npm:pkg@^2.0.0") {
			ParsedSource::Npm {
				name,
				range,
				pinned,
				..
			} => {
				assert_eq!(name, "pkg");
				assert_eq!(range.as_deref(), Some("^2.0.0"));
				assert!(!pinned);
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn git_urls_normalize_host_path_and_ref() {
		// scp-like with ref — the git: prefix is required (bare scp-like is
		// a local path upstream, isLocalPath swallows it).
		match parse_source("git:git@github.com:user/repo@v1.0") {
			ParsedSource::Git {
				repo,
				host,
				path,
				ref_,
				pinned,
			} => {
				assert_eq!(repo, "git@github.com:user/repo");
				assert_eq!(host, "github.com");
				assert_eq!(path, "user/repo");
				assert_eq!(ref_.as_deref(), Some("v1.0"));
				assert!(pinned);
			}
			other => panic!("{other:?}"),
		}
		// https with .git suffix, no ref.
		match parse_source("https://github.com/user/repo.git") {
			ParsedSource::Git {
				repo,
				host,
				path,
				ref_,
				pinned,
			} => {
				assert_eq!(repo, "https://github.com/user/repo.git");
				assert_eq!(host, "github.com");
				assert_eq!(path, "user/repo");
				assert_eq!(ref_, None);
				assert!(!pinned);
			}
			other => panic!("{other:?}"),
		}
		// Without a scheme prefix a bare shorthand is local (upstream
		// isLocalPath returns true; parseGitUrl never sees it).
		assert!(matches!(
			parse_source("github.com/user/repo"),
			ParsedSource::Local { .. }
		));
		// With git: the shorthand is accepted and rewritten to https.
		match parse_source("git:github.com/user/repo@main") {
			ParsedSource::Git { repo, ref_, .. } => {
				assert_eq!(repo, "https://github.com/user/repo");
				assert_eq!(ref_.as_deref(), Some("main"));
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn local_paths_recognize_relative_and_existing_forms() {
		assert!(matches!(
			parse_source("./ext/index.ts"),
			ParsedSource::Local { .. }
		));
		assert!(matches!(
			parse_source("/abs/path"),
			ParsedSource::Local { .. }
		));
		// A bare existing path is local even without a ./ prefix.
		let dir = std::env::temp_dir().join(format!("pi-pm-local-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		assert!(matches!(
			parse_source(dir.to_str().unwrap()),
			ParsedSource::Local { .. }
		));
		let _ = std::fs::remove_dir(&dir);
	}

	#[test]
	fn package_resource_discovery_follows_the_upstream_rules() {
		let root = std::env::temp_dir().join(format!("pi-pm-res-{}", std::process::id()));
		let pkg = root.join("pkg");
		std::fs::create_dir_all(pkg.join("node_modules/dep")).unwrap();
		std::fs::create_dir_all(pkg.join(".hidden")).unwrap();
		// Convention dirs per upstream collectDefaultResources.
		std::fs::create_dir_all(pkg.join("extensions/tools")).unwrap();
		std::fs::create_dir_all(pkg.join("extensions/node_modules/dep")).unwrap();
		std::fs::create_dir_all(pkg.join("extensions/.hidden")).unwrap();
		std::fs::create_dir_all(pkg.join("skills/nested")).unwrap();
		std::fs::create_dir_all(pkg.join("prompts")).unwrap();
		std::fs::create_dir_all(pkg.join("themes")).unwrap();
		std::fs::write(pkg.join("extensions/util.ts"), "export default 1").unwrap();
		std::fs::write(pkg.join("extensions/tools/index.js"), "export default 2").unwrap();
		std::fs::write(pkg.join("extensions/node_modules/dep/index.ts"), "").unwrap();
		std::fs::write(pkg.join("extensions/.hidden/index.ts"), "").unwrap();
		std::fs::write(pkg.join("skills/nested/SKILL.md"), "# skill").unwrap();
		std::fs::write(pkg.join("prompts/one.md"), "template").unwrap();
		// Upstream's prompts walk takes every *.md, SKILL.md included (the
		// template parser decides what loads).
		std::fs::write(pkg.join("prompts/SKILL.md"), "not a prompt").unwrap();
		std::fs::write(pkg.join("themes/dark.json"), "{}").unwrap();
		// gitignore rules prune matches.
		std::fs::create_dir_all(pkg.join("extensions/skipped")).unwrap();
		std::fs::write(pkg.join("extensions/skipped/index.ts"), "").unwrap();
		// Ignore files are read per scanned directory (upstream
		// addIgnoreRules(ig, dir, root) with dir == the walk root).
		std::fs::write(pkg.join("extensions/.gitignore"), "skipped/\n").unwrap();

		let files = collect_package_files(&pkg);
		let names = |paths: &[PathBuf]| -> Vec<String> {
			paths
				.iter()
				.map(|path| {
					path.strip_prefix(&pkg)
						.unwrap()
						.to_string_lossy()
						.into_owned()
				})
				.collect()
		};
		assert_eq!(
			names(&files.extensions),
			vec![
				"extensions/tools/index.js".to_string(),
				"extensions/util.ts".to_string()
			]
		);
		assert_eq!(
			names(&files.skills),
			vec!["skills/nested/SKILL.md".to_string()]
		);
		assert_eq!(
			names(&files.prompts),
			vec!["prompts/SKILL.md".to_string(), "prompts/one.md".to_string()]
		);
		assert_eq!(names(&files.themes), vec!["themes/dark.json".to_string()]);
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn manifest_declared_entries_win_over_the_convention_dirs() {
		let root = std::env::temp_dir().join(format!("pi-pm-manifest-{}", std::process::id()));
		let pkg = root.join("pkg");
		std::fs::create_dir_all(pkg.join("extensions")).unwrap();
		std::fs::create_dir_all(pkg.join("prompts")).unwrap();
		std::fs::write(pkg.join("extensions/auto.ts"), "").unwrap();
		std::fs::write(pkg.join("custom.ts"), "").unwrap();
		std::fs::write(pkg.join("prompts/auto.md"), "").unwrap();
		std::fs::write(pkg.join("docs.md"), "").unwrap();
		std::fs::write(
			pkg.join("package.json"),
			serde_json::json!({"pi": {"extensions": ["custom.ts"], "prompts": ["docs.md"]}})
				.to_string(),
		)
		.unwrap();
		let files = collect_package_files(&pkg);
		assert_eq!(files.extensions, vec![pkg.join("custom.ts")]);
		assert_eq!(files.prompts, vec![pkg.join("docs.md")]);
		let _ = std::fs::remove_dir_all(&root);
	}

	/// Serializes tests that repoint `PI_RS_AGENT_DIR` (cargo runs tests in
	/// threads within one process; the env var is process-global). A tokio
	/// mutex because the async round trips hold it across awaits.
	static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

	#[test]
	fn pattern_filters_follow_the_upstream_order() {
		let root = std::env::temp_dir().join(format!("pi-pm-pat-{}", std::process::id()));
		std::fs::create_dir_all(root.join("skills/nested")).unwrap();
		std::fs::create_dir_all(root.join("extensions/tools")).unwrap();
		let util = root.join("extensions/util.ts");
		let tool = root.join("extensions/tools/index.js");
		let skill = root.join("skills/nested/SKILL.md");
		for path in [&util, &tool, &skill] {
			std::fs::write(path, "").unwrap();
		}
		let files = vec![util.clone(), tool.clone(), skill.clone()];
		let enabled = |patterns: &[String]| -> Vec<String> {
			let mut names: Vec<String> = apply_patterns(&files, patterns, &root)
				.into_iter()
				.map(|path| relative_posix(&root, &path))
				.collect();
			names.sort();
			names
		};

		// No patterns → everything enabled.
		assert_eq!(
			enabled(&[]),
			vec![
				"extensions/tools/index.js",
				"extensions/util.ts",
				"skills/nested/SKILL.md"
			]
		);
		// Plain patterns include by glob (rel path, basename, or full path).
		assert_eq!(
			enabled(&["extensions/*.ts".into()]),
			vec!["extensions/util.ts"]
		);
		assert_eq!(enabled(&["util.ts".into()]), vec!["extensions/util.ts"]);
		// `!` excludes by glob; a SKILL.md also matches its parent dir forms.
		assert_eq!(
			enabled(&["!skills/nested".into()]),
			vec!["extensions/tools/index.js", "extensions/util.ts"]
		);
		// `+` force-includes exactly and beats `!`; `-` force-excludes exactly
		// and beats `+`.
		assert_eq!(
			enabled(&["!skills".into(), "+./skills/nested/SKILL.md".into()]),
			vec![
				"extensions/tools/index.js",
				"extensions/util.ts",
				"skills/nested/SKILL.md"
			]
		);
		// The force-exclude removes only the skill — the extensions were
		// never touched.
		assert_eq!(
			enabled(&[
				"+skills/nested/SKILL.md".into(),
				"-skills/nested/SKILL.md".into()
			]),
			vec!["extensions/tools/index.js", "extensions/util.ts"]
		);
		// Exact patterns also accept the SKILL parent dir form.
		assert_eq!(
			enabled(&["!skills/**".into(), "+nested".into()]),
			vec![
				"extensions/tools/index.js",
				"extensions/util.ts",
				"skills/nested/SKILL.md"
			]
		);
		let _ = std::fs::remove_dir_all(&root);
	}

	#[test]
	fn override_state_reads_project_entries_like_upstream() {
		let entry = |value: Value| value;
		// Missing array inherits; explicit empty array unloads; an
		// autoload:false shell with an empty array inherits.
		assert_eq!(
			project_override_state(&entry(json!({"source": "x"})), ResourceType::Skills, "s"),
			None
		);
		assert_eq!(
			project_override_state(
				&entry(json!({"source": "x", "skills": []})),
				ResourceType::Skills,
				"s"
			),
			Some(false)
		);
		assert_eq!(
			project_override_state(
				&entry(json!({"source": "x", "autoload": false, "skills": []})),
				ResourceType::Skills,
				"s"
			),
			None
		);
		// `+` loads, `-`/`!` unloads, later entries win, unrelated targets
		// inherit, `./` prefixes normalize.
		let patterns = json!({"source": "x", "skills": ["+./SKILLS.md", "-other.md"]});
		assert_eq!(
			project_override_state(&entry(patterns), ResourceType::Skills, "SKILLS.md"),
			Some(true)
		);
		let patterns =
			json!({"source": "x", "skills": ["!skills/a/SKILL.md", "+skills/a/SKILL.md"]});
		assert_eq!(
			project_override_state(&entry(patterns), ResourceType::Skills, "skills/a/SKILL.md"),
			Some(true)
		);
		let patterns = json!({"source": "x", "skills": ["-skills/a/SKILL.md"]});
		assert_eq!(
			project_override_state(&entry(patterns), ResourceType::Skills, "skills/b/SKILL.md"),
			None
		);
	}

	#[test]
	fn package_patterns_round_trip_through_settings() {
		// The global settings path comes from `PI_RS_AGENT_DIR`
		// (`config::settings_json_path`), so the env var must point at the
		// fixture agent dir — serialized with the other env-touching tests.
		let _guard = futures::executor::block_on(ENV_LOCK.lock());
		let root = std::env::temp_dir().join(format!(
			"pi-pm-rt-{}-{}",
			std::process::id(),
			pi_ai::types::now_ms()
		));
		let agent_dir = root.join("agent");
		let cwd = root.join("project");
		let pkg = root.join("pkg");
		std::fs::create_dir_all(&agent_dir).unwrap();
		std::fs::create_dir_all(&cwd).unwrap();
		std::fs::create_dir_all(pkg.join("extensions")).unwrap();
		std::fs::create_dir_all(pkg.join("skills/nested")).unwrap();
		std::fs::write(pkg.join("extensions/util.ts"), "").unwrap();
		std::fs::write(pkg.join("skills/nested/SKILL.md"), "# s").unwrap();
		let source = pkg.display().to_string();
		let settings = agent_dir.join("settings.json");
		std::fs::write(&settings, json!({"packages": [source]}).to_string()).unwrap();
		let previous = std::env::var(crate::config::AGENT_DIR_ENV).ok();
		unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, &agent_dir) };

		let result = package_patterns_round_trip_body(&cwd, &agent_dir, &pkg, &source, &settings);
		match previous {
			Some(value) => unsafe { std::env::set_var(crate::config::AGENT_DIR_ENV, value) },
			None => unsafe { std::env::remove_var(crate::config::AGENT_DIR_ENV) },
		}
		result.unwrap();
		let _ = std::fs::remove_dir_all(&root);
	}

	fn package_patterns_round_trip_body(
		cwd: &Path,
		agent_dir: &Path,
		pkg: &Path,
		source: &str,
		settings: &Path,
	) -> Result<(), String> {
		let manager = PackageManager::new(cwd, agent_dir, true);
		let find = |items: &[ResourceItem], ty: ResourceType| {
			items
				.iter()
				.find(|item| item.resource_type == ty)
				.unwrap()
				.clone()
		};
		let (items, warnings) = manager.resolve_items();
		assert!(warnings.is_empty());
		assert!(find(&items, ResourceType::Extensions).enabled);
		assert!(find(&items, ResourceType::Skills).enabled);
		assert_eq!(
			find(&items, ResourceType::Extensions).project_override,
			None
		);

		// Disable one extension globally: entry promoted to an object with a
		// `-` pattern.
		manager.set_package_patterns(
			source,
			ResourceType::Extensions,
			Some(&["-extensions/util.ts".to_string()]),
			false,
		)?;
		let (items, _) = manager.resolve_items();
		assert!(!find(&items, ResourceType::Extensions).enabled);
		assert_eq!(
			read_entries(settings),
			vec![json!({"source": source, "extensions": ["-extensions/util.ts"]})]
		);

		// Project override: force-load the skill from the project settings —
		// an autoload:false shell entry appears there.
		let project_settings = crate::config::project_pi_dir(cwd).join("settings.json");
		manager.set_package_patterns(
			source,
			ResourceType::Skills,
			Some(&["+skills/nested/SKILL.md".to_string()]),
			true,
		)?;
		let (items, _) = manager.resolve_items();
		let skill = find(&items, ResourceType::Skills);
		assert!(
			skill.enabled,
			"the defining (global) entry still enables it"
		);
		assert_eq!(skill.project_override, Some(true));
		let extension = find(&items, ResourceType::Extensions);
		assert_eq!(
			extension.project_override, None,
			"types without an array in the project shell inherit"
		);
		assert_eq!(
			read_entries(&project_settings),
			vec![json!({
				"source": source,
				"autoload": false,
				"skills": ["+skills/nested/SKILL.md"],
			})]
		);

		// Startup keeps only effectively-enabled files.
		let resources = manager.resolve_resources();
		assert!(resources.extensions.is_empty());
		assert_eq!(resources.skills, vec![pkg.join("skills/nested/SKILL.md")]);

		// Clearing the global array collapses the object back to a string;
		// clearing the override-only project entry removes it.
		manager.set_package_patterns(source, ResourceType::Extensions, None, false)?;
		manager.set_package_patterns(source, ResourceType::Skills, None, true)?;
		assert_eq!(read_entries(settings), vec![json!(source)]);
		assert!(read_entries(&project_settings).is_empty());
		let (items, _) = manager.resolve_items();
		assert!(find(&items, ResourceType::Extensions).enabled);
		assert_eq!(find(&items, ResourceType::Skills).project_override, None);
		Ok(())
	}
}
